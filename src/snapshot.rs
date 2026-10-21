use dfs_daemon::fs::inode::{FileId, InodeKind, InodeMeta, SyncEvent, VersionVector};
use dfs_daemon::store::metadata::{DentryRecord, MetaSnapshot};
use dfs_daemon::store::s3::S3Store;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Upper bound for a decompressed metadata snapshot.
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

pub struct SnapshotCache {
    pub snapshot: Arc<ParsedSnapshot>,
    pub loaded_at: Instant,
}

impl SnapshotCache {
    pub fn age_secs(&self) -> u64 {
        self.loaded_at.elapsed().as_secs()
    }
}

/// One device snapshot found under `meta/snapshots/`.
#[derive(Clone, Serialize)]
pub struct DeviceInfo {
    pub device_id: Uuid,
    pub snapshot_ms: u64,
    pub snapshot_bytes: usize,
    pub inodes: usize,
    pub dentries: usize,
}

/// In-memory view of the DFS filesystem built from all device snapshots.
pub struct ParsedSnapshot {
    pub inodes: HashMap<FileId, InodeMeta>,
    /// parent_id -> Vec<(name, child_id)>
    pub children: HashMap<FileId, Vec<(String, FileId)>>,
    pub devices: Vec<DeviceInfo>,
}

impl ParsedSnapshot {
    /// Download all device snapshots from S3, merge them, and build the in-memory tree.
    /// For each inode the copy with the newest mtime wins.
    pub async fn load_from_s3(s3: &S3Store) -> anyhow::Result<Self> {
        let keys = s3.list_objects_with_prefix("meta/snapshots/").await?;

        let mut inodes: HashMap<FileId, InodeMeta> = HashMap::new();
        let mut children: HashMap<FileId, Vec<(String, FileId)>> = HashMap::new();
        let mut devices = Vec::new();

        // The root directory always exists, even in an empty bucket.
        children.entry(Uuid::nil()).or_default();

        for key in keys {
            if !key.ends_with(".msgpack.zstd") {
                continue;
            }
            let data = match s3.get_object(&key).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("snapshot {key}: {e:#}");
                    continue;
                }
            };
            let snapshot = match decode_snapshot(&data, MAX_SNAPSHOT_BYTES) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("snapshot {key}: {e:#}");
                    continue;
                }
            };

            devices.push(DeviceInfo {
                device_id: snapshot.device_id,
                snapshot_ms: snapshot.epoch_ms,
                snapshot_bytes: data.len(),
                inodes: snapshot.inodes.len(),
                dentries: snapshot.dentries.len(),
            });

            for inode in snapshot.inodes {
                if inode.tombstone {
                    continue;
                }
                let newer = inodes
                    .get(&inode.id)
                    .map(|existing| inode.mtime > existing.mtime)
                    .unwrap_or(true);
                if newer {
                    inodes.insert(inode.id, inode);
                }
            }
            for rec in snapshot.dentries {
                let list = children.entry(rec.parent_id).or_default();
                if !list.iter().any(|(n, _)| n == &rec.name) {
                    list.push((rec.name, rec.child_id));
                }
            }
        }

        devices.sort_by_key(|d| std::cmp::Reverse(d.snapshot_ms));
        Ok(Self {
            inodes,
            children,
            devices,
        })
    }

    /// Walk path components from the root and return the FileId.
    pub fn resolve(&self, path: &str) -> Option<FileId> {
        let mut current = Uuid::nil();
        for part in path.split('/').filter(|s| !s.is_empty()) {
            current = self
                .children
                .get(&current)?
                .iter()
                .find(|(name, _)| name == part)
                .map(|(_, id)| *id)?;
        }
        Some(current)
    }

    /// List the children of a directory. Directories first, then by name.
    pub fn list_dir(&self, path: &str) -> Option<Vec<DirEntry>> {
        let dir_id = self.resolve(path)?;
        let children = self.children.get(&dir_id)?;
        let mut entries: Vec<DirEntry> = children
            .iter()
            .filter_map(|(name, child_id)| {
                let inode = self.inodes.get(child_id)?;
                Some(DirEntry {
                    name: name.clone(),
                    kind: inode.kind,
                    size: inode.size,
                    mtime_ms: system_time_to_ms(inode.mtime),
                    atime_ms: system_time_to_ms(inode.atime),
                    ctime_ms: system_time_to_ms(inode.ctime),
                    devices: short_device_ids(&inode.version_vector),
                    encrypted: inode.chunks.iter().any(|c| c.encrypted),
                    chunks: inode.chunks.len(),
                    id: *child_id,
                })
            })
            .collect();
        entries.sort_by(|a, b| match (&a.kind, &b.kind) {
            (InodeKind::Directory, InodeKind::Directory) => a.name.cmp(&b.name),
            (InodeKind::Directory, _) => std::cmp::Ordering::Less,
            (_, InodeKind::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        Some(entries)
    }
}

pub fn decode_snapshot(data: &[u8], limit: usize) -> anyhow::Result<MetaSnapshot> {
    let raw = zstd::bulk::decompress(data, limit)?;
    Ok(rmp_serde::from_slice(&raw)?)
}

pub fn encode_snapshot(snap: &MetaSnapshot) -> anyhow::Result<Vec<u8>> {
    let raw = rmp_serde::to_vec(snap)?;
    Ok(zstd::bulk::compress(&raw, 3)?)
}

/// Short IDs (first 8 hex chars) of the devices in a version vector, sorted.
pub fn short_device_ids(vv: &VersionVector) -> Vec<String> {
    let mut ids: Vec<String> = vv
        .clocks
        .keys()
        .map(|id| id.to_string()[..8].to_string())
        .collect();
    ids.sort();
    ids
}

/// Write a single new file (inode + dentry) into the browser's device snapshot on S3.
/// Reads the existing browser snapshot first so repeated uploads accumulate correctly.
pub async fn write_file_to_browser_snapshot(
    s3: &S3Store,
    browser_device_id: Uuid,
    inode: InodeMeta,
    parent_id: FileId,
    name: String,
) -> anyhow::Result<()> {
    let key = S3Store::snapshot_key(browser_device_id);

    let (mut inodes, mut dentries) = match s3.get_object(&key).await {
        Ok(data) => match decode_snapshot(&data, 64 * 1024 * 1024) {
            Ok(snap) => (snap.inodes, snap.dentries),
            Err(_) => (Vec::new(), Vec::new()),
        },
        Err(_) => (Vec::new(), Vec::new()),
    };

    inodes.retain(|i| i.id != inode.id);
    inodes.push(inode.clone());

    dentries.retain(|d: &DentryRecord| !(d.parent_id == parent_id && d.name == name));
    dentries.push(DentryRecord {
        parent_id,
        name: name.clone(),
        child_id: inode.id,
    });

    let snap = MetaSnapshot {
        device_id: browser_device_id,
        epoch_ms: system_time_to_ms(SystemTime::now()),
        inodes,
        dentries,
    };
    s3.put_snapshot(browser_device_id, &encode_snapshot(&snap)?)
        .await?;

    // Also emit a FileCreate delta so running daemons pick up the file without
    // a remount (they poll meta/deltas/, not meta/snapshots/).
    let event = SyncEvent::FileCreate {
        file_id: inode.id,
        parent_id,
        name,
        device_id: browser_device_id,
        seq: 0, // the sequence lives in the delta key
        vv: VersionVector::new(),
        inode,
    };
    let bytes = rmp_serde::to_vec(&event)?;
    // A millisecond timestamp as sequence number keeps S3 key order == event order.
    let ts_seq = system_time_to_ms(SystemTime::now());
    s3.put_delta(browser_device_id, ts_seq, &bytes).await?;

    Ok(())
}

#[derive(Serialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: InodeKind,
    pub size: u64,
    pub mtime_ms: u64,
    pub atime_ms: u64,
    pub ctime_ms: u64,
    /// Short IDs (first 8 chars) of devices that have written this file.
    pub devices: Vec<String>,
    pub encrypted: bool,
    pub chunks: usize,
    pub id: FileId,
}

pub fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}
