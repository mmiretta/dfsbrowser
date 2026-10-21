//! Seed an S3 bucket with a small, entirely synthetic DFS volume.
//!
//! The volume uses the real DFS formats (device snapshots, 4 MiB chunks,
//! Reed-Solomon shards, AES-256-GCM encrypted chunks, saved versions), so the
//! browser can be tried without mounting DFS anywhere.
//!
//! Environment:
//!   DFS_DEMO_ENDPOINT        S3 endpoint            (default http://localhost:9000)
//!   DFS_DEMO_BUCKET          bucket                 (default dfs)
//!   DFS_DEMO_ACCESS_KEY      access key             (required)
//!   DFS_DEMO_SECRET_KEY      secret key             (required)
//!   DFS_DEMO_MASTER_KEY_HEX  key for the encrypted folder (default: the key in demo/master.key)
//!   DFS_DEMO_FORCE           re-seed even if the bucket already has snapshots

use dfs_daemon::config::S3Config;
use dfs_daemon::fs::inode::{
    ChunkLocation, ChunkRef, FileId, InodeKind, InodeMeta, ShardLocation, VersionVector,
};
use dfs_daemon::store::metadata::{DentryRecord, MetaSnapshot};
use dfs_daemon::store::s3::S3Store;
use dfs_daemon::store::{crypto, erasure};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// SHA-256("dfs-browser demo master key"); the same 32 bytes as demo/master.key.
const DEMO_MASTER_KEY_HEX: &str =
    "ddf0eee0f4136d890aa127460c49e57d4bf37dbfb9c071a3e9fa9dc7f3d34f0d";

const LAPTOP: Uuid = Uuid::from_u128(0x0192_a4c1_7e3b_7d10_9c41_5a2e_0f6d_1001);
const DESKTOP: Uuid = Uuid::from_u128(0x0192_b77f_02aa_7c55_8e10_3b9d_44c2_2002);
const NAS: Uuid = Uuid::from_u128(0x0192_c3d9_51e0_7a08_b6f7_19ce_88a1_3003);

/// How a file's chunks are stored.
#[derive(Clone, Copy)]
enum Layout {
    Plain,
    Encrypted,
    /// Reed-Solomon with k data + m parity shards.
    Erasure(u8, u8),
}

struct Seeder {
    s3: S3Store,
    key: [u8; 32],
    now: SystemTime,
    inodes: Vec<InodeMeta>,
    dentries: Vec<DentryRecord>,
    /// Which devices hold each inode in their snapshot.
    holders: HashMap<FileId, Vec<Uuid>>,
    rng: u64,
}

impl Seeder {
    fn ago(&self, days: f64) -> SystemTime {
        self.now - Duration::from_secs_f64(days * 86400.0)
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64*, deterministic so every demo looks the same.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn random_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next_rand().to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn vv(writes: &[(Uuid, u64)]) -> VersionVector {
        VersionVector {
            clocks: writes.iter().copied().collect(),
        }
    }

    fn base_inode(kind: InodeKind, mtime: SystemTime, vv: VersionVector) -> InodeMeta {
        InodeMeta {
            id: Uuid::now_v7(),
            kind,
            size: 0,
            atime: mtime,
            mtime,
            ctime: mtime,
            mode: if kind == InodeKind::Directory {
                0o755
            } else {
                0o644
            },
            uid: 1000,
            gid: 1000,
            nlink: if kind == InodeKind::Directory { 2 } else { 1 },
            chunks: Vec::new(),
            version_vector: vv,
            tombstone: false,
            tombstone_time: None,
            symlink_target: None,
            xattrs: HashMap::new(),
        }
    }

    fn link(&mut self, parent: FileId, name: &str, inode: InodeMeta, holders: &[Uuid]) -> FileId {
        let id = inode.id;
        self.dentries.push(DentryRecord {
            parent_id: parent,
            name: name.into(),
            child_id: id,
        });
        self.holders.insert(id, holders.to_vec());
        self.inodes.push(inode);
        id
    }

    fn dir(&mut self, parent: FileId, name: &str, days_ago: f64, holders: &[Uuid]) -> FileId {
        let inode = Self::base_inode(
            InodeKind::Directory,
            self.ago(days_ago),
            Self::vv(&[(holders[0], 1)]),
        );
        self.link(parent, name, inode, holders)
    }

    async fn store_chunks(&mut self, data: &[u8], layout: Layout) -> anyhow::Result<Vec<ChunkRef>> {
        let mut refs = Vec::new();
        for (i, part) in data.chunks(CHUNK_SIZE).enumerate() {
            let chunk_id = Uuid::now_v7();
            let checksum: [u8; 32] = Sha256::digest(part).into();
            let (location, encrypted) = match layout {
                Layout::Plain | Layout::Encrypted => {
                    let encrypted = matches!(layout, Layout::Encrypted);
                    let bytes = if encrypted {
                        crypto::encrypt(chunk_id, part, &self.key)?
                    } else {
                        part.to_vec()
                    };
                    let s3_key = self.s3.put_chunk(chunk_id, &bytes).await?;
                    let shards = vec![ShardLocation {
                        node_id: Uuid::nil(),
                        endpoint: self.s3.endpoint.clone(),
                        shard_index: 0,
                        s3_key,
                    }];
                    (
                        ChunkLocation::Distributed {
                            shards,
                            ec_k: 1,
                            ec_m: 0,
                        },
                        encrypted,
                    )
                }
                Layout::Erasure(k, m) => {
                    let mut shards = Vec::new();
                    for (idx, shard) in erasure::encode(part, k as usize, m as usize)?
                        .iter()
                        .enumerate()
                    {
                        let s3_key = self.s3.put_shard(chunk_id, idx as u8, shard).await?;
                        shards.push(ShardLocation {
                            // One storage node per shard in a real deployment; the demo
                            // keeps every shard in the same bucket.
                            node_id: Uuid::from_u128(
                                0x0192_d000_0000_7000_8000_0000_0000_0000 + idx as u128,
                            ),
                            endpoint: self.s3.endpoint.clone(),
                            shard_index: idx as u8,
                            s3_key,
                        });
                    }
                    (
                        ChunkLocation::Distributed {
                            shards,
                            ec_k: k,
                            ec_m: m,
                        },
                        false,
                    )
                }
            };
            refs.push(ChunkRef {
                chunk_id,
                offset: (i * CHUNK_SIZE) as u64,
                length: part.len() as u32,
                checksum,
                location,
                encrypted,
            });
        }
        Ok(refs)
    }

    #[allow(clippy::too_many_arguments)]
    async fn file(
        &mut self,
        parent: FileId,
        name: &str,
        data: &[u8],
        layout: Layout,
        days_ago: f64,
        writes: &[(Uuid, u64)],
        holders: &[Uuid],
    ) -> anyhow::Result<FileId> {
        let mut inode = Self::base_inode(InodeKind::File, self.ago(days_ago), Self::vv(writes));
        inode.atime = self.ago(days_ago / 3.0);
        inode.ctime = self.ago(days_ago * 1.5 + 2.0);
        inode.size = data.len() as u64;
        inode.chunks = self.store_chunks(data, layout).await?;
        Ok(self.link(parent, name, inode, holders))
    }

    /// Store older contents of `file_id` as saved versions (newest last).
    async fn versions(
        &mut self,
        file_id: FileId,
        device: Uuid,
        history: &[(&str, f64)],
        layout: Layout,
    ) -> anyhow::Result<()> {
        let current = self
            .inodes
            .iter()
            .find(|i| i.id == file_id)
            .cloned()
            .expect("inode exists");
        for (seq, (content, days_ago)) in history.iter().enumerate() {
            let mut v = current.clone();
            v.size = content.len() as u64;
            v.mtime = self.ago(*days_ago);
            v.chunks = self.store_chunks(content.as_bytes(), layout).await?;
            let key = format!("meta/versions/{device}/{file_id}/{:020}.msgpack", seq + 1);
            self.s3
                .put_object_raw(&key, &rmp_serde::to_vec(&v)?)
                .await?;
        }
        Ok(())
    }

    async fn write_snapshots(&self) -> anyhow::Result<()> {
        for device in [LAPTOP, DESKTOP, NAS] {
            let held = |id: &FileId| self.holders.get(id).is_some_and(|h| h.contains(&device));
            let snap = MetaSnapshot {
                device_id: device,
                epoch_ms: ms(self.now) - (device.as_u128() as u64 % 7) * 3_600_000,
                inodes: self
                    .inodes
                    .iter()
                    .filter(|i| held(&i.id))
                    .cloned()
                    .collect(),
                dentries: self
                    .dentries
                    .iter()
                    .filter(|d| held(&d.child_id))
                    .map(|d| DentryRecord {
                        parent_id: d.parent_id,
                        name: d.name.clone(),
                        child_id: d.child_id,
                    })
                    .collect(),
            };
            let raw = rmp_serde::to_vec(&snap)?;
            self.s3
                .put_snapshot(device, &zstd::bulk::compress(&raw, 3)?)
                .await?;
            println!("snapshot for device {device}: {} inodes", snap.inodes.len());
        }
        Ok(())
    }
}

fn ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn env(name: &str, default: Option<&str>) -> anyhow::Result<String> {
    match std::env::var(name).ok().filter(|v| !v.is_empty()) {
        Some(v) => Ok(v),
        None => default
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("{name} is required")),
    }
}

fn svg(title: &str, hue: u32, seed: u64) -> String {
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"960\" height=\"640\" viewBox=\"0 0 960 640\">\
<defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
<stop offset=\"0\" stop-color=\"hsl({hue},70%,62%)\"/><stop offset=\"1\" stop-color=\"hsl({},60%,22%)\"/>\
</linearGradient></defs><rect width=\"960\" height=\"640\" fill=\"url(#g)\"/>",
        (hue + 40) % 360
    );
    s += &format!(
        "<circle cx=\"{}\" cy=\"170\" r=\"70\" fill=\"hsl({},90%,85%)\" opacity=\".9\"/>",
        180 + seed % 600,
        (hue + 180) % 360
    );
    for layer in 0..3u64 {
        let mut d = format!("M0 640 L0 {}", 380 + layer * 60);
        for x in (0..=960).step_by(80) {
            let y = 360 + layer * 70 + ((seed.wrapping_mul(31 + x as u64 + layer * 7)) % 90);
            d += &format!(" L{x} {y}");
        }
        d += " L960 640 Z";
        s += &format!(
            "<path d=\"{d}\" fill=\"hsl({},45%,{}%)\" opacity=\".85\"/>",
            (hue + 200) % 360,
            30 - layer * 8
        );
    }
    s += &format!("<text x=\"32\" y=\"600\" font-family=\"sans-serif\" font-size=\"28\" fill=\"#fff\" opacity=\".85\">{title}</text></svg>");
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = S3Config {
        endpoint: env("DFS_DEMO_ENDPOINT", Some("http://localhost:9000"))?,
        bucket: env("DFS_DEMO_BUCKET", Some("dfs"))?,
        access_key: env("DFS_DEMO_ACCESS_KEY", None)?,
        secret_key: env("DFS_DEMO_SECRET_KEY", None)?,
        region: "us-east-1".into(),
    };
    let key: [u8; 32] = hex::decode(env("DFS_DEMO_MASTER_KEY_HEX", Some(DEMO_MASTER_KEY_HEX))?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("master key must be 32 bytes"))?;

    // The object store may still be starting.
    let mut attempt = 0;
    let s3 = loop {
        match S3Store::new(&cfg).await {
            Ok(s) => break s,
            Err(e) if attempt < 30 => {
                attempt += 1;
                eprintln!("waiting for {} ({e:#})", cfg.endpoint);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    };

    let force = std::env::var("DFS_DEMO_FORCE").is_ok_and(|v| !v.is_empty() && v != "0");
    if !force
        && !s3
            .list_objects_with_prefix("meta/snapshots/")
            .await?
            .is_empty()
    {
        println!(
            "bucket {} already holds DFS snapshots; nothing to do",
            cfg.bucket
        );
        return Ok(());
    }

    let mut sd = Seeder {
        s3,
        key,
        now: SystemTime::now(),
        inodes: Vec::new(),
        dentries: Vec::new(),
        holders: HashMap::new(),
        rng: 0x9E37_79B9_7F4A_7C15,
    };
    let all = [LAPTOP, DESKTOP, NAS];
    let lap_desk = [LAPTOP, DESKTOP];
    let root = Uuid::nil();

    // ── documents ──
    let docs = sd.dir(root, "documents", 120.0, &all);
    sd.file(
        docs,
        "README.md",
        include_bytes!("../../demo/content/README.md"),
        Layout::Plain,
        40.0,
        &[(LAPTOP, 3)],
        &all,
    )
    .await?;
    let arch = sd
        .file(
            docs,
            "architecture.md",
            include_bytes!("../../demo/content/architecture.md"),
            Layout::Plain,
            2.3,
            &[(LAPTOP, 7), (DESKTOP, 4)],
            &all,
        )
        .await?;
    sd.versions(
        arch,
        LAPTOP,
        &[
            ("# Home lab architecture\n\nDraft.\n", 30.0),
            ("# Home lab architecture\n\nTwo MinIO nodes, one bucket.\n\n- laptop\n- desktop\n", 18.0),
            ("# Home lab architecture\n\nThree MinIO nodes with 4+2 erasure coding.\n\n- laptop\n- desktop\n- nas\n", 9.0),
        ],
        Layout::Plain,
    )
    .await?;
    sd.versions(
        arch,
        DESKTOP,
        &[(
            "# Home lab architecture\n\nThree nodes, 4+2, nightly mirror to a cloud bucket.\n",
            5.0,
        )],
        Layout::Plain,
    )
    .await?;
    let notes = sd.dir(docs, "meeting-notes", 60.0, &lap_desk);
    for (i, (day, topic)) in [
        ("2026-09-02", "storage budget"),
        ("2026-09-16", "backup drill"),
        ("2026-10-07", "mirror rollout"),
    ]
    .iter()
    .enumerate()
    {
        let body = format!(
            "# Notes {day}: {topic}\n\nAttendees: Alice, Bob, Carol\n\n## Decisions\n\n- Keep 30 days of versions\n- Review shard placement monthly\n\n## Actions\n\n- [ ] Alice: update the runbook\n- [x] Bob: rotate the demo credentials\n"
        );
        sd.file(
            notes,
            &format!("{day}.md"),
            body.as_bytes(),
            Layout::Plain,
            45.0 - i as f64 * 14.0,
            &[(DESKTOP, 1 + i as u64)],
            &lap_desk,
        )
        .await?;
    }

    // ── photos (SVG so the demo stays small) ──
    let photos = sd.dir(root, "photos", 200.0, &lap_desk);
    let trip = sd.dir(photos, "2026-coast-trip", 75.0, &lap_desk);
    for (i, name) in [
        "harbour-dawn",
        "lighthouse",
        "dunes",
        "old-town",
        "sunset-pier",
    ]
    .iter()
    .enumerate()
    {
        let image = svg(name, 190 + i as u32 * 31, 17 + i as u64 * 97);
        sd.file(
            trip,
            &format!("{name}.svg"),
            image.as_bytes(),
            Layout::Plain,
            74.0 - i as f64,
            &[(LAPTOP, 1)],
            &lap_desk,
        )
        .await?;
    }

    // ── projects ──
    let projects = sd.dir(root, "projects", 90.0, &all);
    let tele = sd.dir(projects, "telemetry", 50.0, &all);
    let src = sd.dir(tele, "src", 50.0, &all);
    sd.file(
        src,
        "main.rs",
        include_bytes!("../../demo/content/main.rs"),
        Layout::Plain,
        1.2,
        &[(DESKTOP, 12), (LAPTOP, 5)],
        &all,
    )
    .await?;
    sd.file(tele, "Cargo.toml", b"[package]\nname = \"telemetry\"\nversion = \"0.3.1\"\nedition = \"2021\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n", Layout::Plain, 20.0, &[(DESKTOP, 2)], &all).await?;
    let mut csv = String::from("timestamp,sensor,temperature_c,humidity_pct\n");
    for i in 0..1500u64 {
        let r = sd.next_rand();
        csv += &format!(
            "2026-10-{:02}T{:02}:{:02}:00Z,sensor-{},{:.1},{:.0}\n",
            1 + i / 96 % 28,
            i / 4 % 24,
            i % 4 * 15,
            r % 4,
            14.0 + (r % 120) as f64 / 10.0,
            40 + r % 35
        );
    }
    sd.file(
        tele,
        "readings.csv",
        csv.as_bytes(),
        Layout::Plain,
        3.0,
        &[(NAS, 40)],
        &all,
    )
    .await?;
    sd.file(tele, "config.json", b"{\n  \"interval_secs\": 900,\n  \"sensors\": [\"sensor-0\", \"sensor-1\", \"sensor-2\", \"sensor-3\"],\n  \"upload\": { \"bucket\": \"telemetry\", \"retries\": 3 }\n}\n", Layout::Plain, 20.0, &[(DESKTOP, 1)], &all).await?;

    // ── encrypted folder ──
    let private = sd.dir(root, "private", 150.0, &lap_desk);
    let budget = sd
        .file(private, "budget-2026.csv", b"month,income,rent,food,travel,savings\nJan,4200,1300,520,180,900\nFeb,4200,1300,480,90,1000\nMar,4350,1300,505,640,600\nApr,4350,1300,470,120,1100\n", Layout::Encrypted, 6.0, &[(LAPTOP, 9)], &lap_desk)
        .await?;
    sd.versions(
        budget,
        LAPTOP,
        &[
            ("month,income,rent,food\nJan,4200,1300,520\n", 60.0),
            (
                "month,income,rent,food,travel\nJan,4200,1300,520,180\nFeb,4200,1300,480,90\n",
                31.0,
            ),
        ],
        Layout::Encrypted,
    )
    .await?;
    sd.file(
        private,
        "passport-scan.txt",
        b"(placeholder for a scanned document)\n",
        Layout::Encrypted,
        140.0,
        &[(LAPTOP, 1)],
        &lap_desk,
    )
    .await?;
    sd.file(
        private,
        "recovery-codes.txt",
        b"These are fake recovery codes for the demo:\n1111-2222\n3333-4444\n5555-6666\n",
        Layout::Encrypted,
        95.0,
        &[(LAPTOP, 2)],
        &lap_desk,
    )
    .await?;

    // ── large, erasure-coded files on the NAS ──
    let backups = sd.dir(root, "backups", 100.0, &all);
    let big = sd.random_bytes(9 * 1024 * 1024 + 123_457);
    sd.file(
        backups,
        "photos-2026-10-01.tar.zst",
        &big,
        Layout::Erasure(4, 2),
        20.0,
        &[(NAS, 3)],
        &all,
    )
    .await?;
    let small = sd.random_bytes(2 * 1024 * 1024 + 4242);
    sd.file(
        backups,
        "notes-2026-09-01.tar.zst",
        &small,
        Layout::Erasure(4, 2),
        50.0,
        &[(NAS, 2)],
        &all,
    )
    .await?;
    let media = sd.dir(root, "media", 100.0, &[NAS, DESKTOP]);
    let audio = sd.random_bytes(5 * 1024 * 1024 + 777);
    sd.file(
        media,
        "field-recording-12.flac",
        &audio,
        Layout::Erasure(2, 1),
        33.0,
        &[(NAS, 1), (DESKTOP, 1)],
        &[NAS, DESKTOP],
    )
    .await?;

    // A symlink, to show that DFS keeps them.
    let mut link = Seeder::base_inode(InodeKind::Symlink, sd.ago(20.0), Seeder::vv(&[(NAS, 1)]));
    link.symlink_target = Some("photos-2026-10-01.tar.zst".into());
    link.size = 25;
    sd.link(backups, "latest.tar.zst", link, &all);

    sd.file(
        root,
        "todo.txt",
        b"- renew the domain\n- test restore from the cloud mirror\n- label the NAS disks\n",
        Layout::Plain,
        0.4,
        &[(LAPTOP, 21), (DESKTOP, 18)],
        &all,
    )
    .await?;

    sd.write_snapshots().await?;
    println!("demo volume ready in bucket {}", cfg.bucket);
    Ok(())
}
