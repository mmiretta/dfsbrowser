// Handlers return early with a ready-made HttpResponse; boxing it buys nothing here.
#![allow(clippy::result_large_err)]

use crate::session::Session;
use crate::snapshot::{
    short_device_ids, system_time_to_ms, write_file_to_browser_snapshot, ParsedSnapshot,
};
use crate::{chunks, AppState, VERSION};
use actix_web::{http::StatusCode, web, HttpRequest, HttpResponse, Responder};
use base64::Engine as _;
use dfs_daemon::config::S3Config;
use dfs_daemon::fs::inode::{
    ChunkLocation, ChunkRef, InodeKind, InodeMeta, ShardLocation, VersionVector,
};
use dfs_daemon::store::s3::S3Store;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

/// Most versions fetched and returned for a single file.
const MAX_VERSIONS: usize = 100;
/// Most chunks described in a `stat` response.
const MAX_STAT_CHUNKS: usize = 200;

fn err(status: StatusCode, msg: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(json!({ "error": msg.into() }))
}

fn internal(context: &str, e: anyhow::Error) -> HttpResponse {
    tracing::warn!("{context}: {e:#}");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{context}: {e:#}"),
    )
}

// ── Static UI and metadata ────────────────────────────────────────────────────

const CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; connect-src 'self'; \
    form-action 'none'; base-uri 'none'; frame-ancestors 'none'";

pub async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .insert_header(("Content-Security-Policy", CSP))
        .insert_header(("Cache-Control", "no-store"))
        .body(include_str!("ui.html"))
}

pub async fn healthz() -> impl Responder {
    HttpResponse::Ok().json(json!({ "ok": true, "version": VERSION }))
}

/// Non-secret settings the login form needs.
pub async fn config(state: web::Data<AppState>) -> impl Responder {
    let s = &state.settings;
    HttpResponse::Ok().json(json!({
        "version": VERSION,
        "default_endpoint": s.default_endpoint,
        "default_bucket": s.default_bucket,
        "default_region": s.default_region,
        "allowed_endpoints": s.allowed_endpoints,
        "read_only": s.read_only,
    }))
}

// ── Sessions ──────────────────────────────────────────────────────────────────

/// Sessions are carried in the `x-dfs-token` header only (never a cookie), so a
/// third-party page cannot make authenticated requests on a user's behalf.
fn session(req: &HttpRequest, state: &AppState) -> Result<Arc<Session>, HttpResponse> {
    req.headers()
        .get("x-dfs-token")
        .and_then(|v| v.to_str().ok())
        .and_then(|t| state.sessions.get(t))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not logged in"))
}

async fn snapshot(sess: &Session) -> Result<Arc<ParsedSnapshot>, HttpResponse> {
    sess.get_snapshot()
        .await
        .map_err(|e| internal("loading metadata", e))
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: Option<String>,
    /// 32-byte master key, hex or base64 (omit for unencrypted filesystems).
    pub master_key_hex: Option<String>,
}

pub async fn login(state: web::Data<AppState>, body: web::Json<LoginRequest>) -> impl Responder {
    let endpoint = body.endpoint.trim().trim_end_matches('/').to_string();
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return err(
            StatusCode::BAD_REQUEST,
            "endpoint must start with http:// or https://",
        );
    }
    if !state.settings.endpoint_allowed(&endpoint) {
        tracing::warn!(
            "login refused: endpoint {endpoint} is not in DFS_BROWSER_ALLOWED_ENDPOINTS"
        );
        return err(
            StatusCode::FORBIDDEN,
            "this S3 endpoint is not allowed on this server",
        );
    }
    if body.bucket.trim().is_empty() || body.access_key.is_empty() || body.secret_key.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bucket, access key and secret key are required",
        );
    }

    let master_key = match parse_master_key(body.master_key_hex.as_deref()) {
        Ok(k) => k,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };

    let cfg = S3Config {
        endpoint: endpoint.clone(),
        bucket: body.bucket.trim().to_string(),
        access_key: body.access_key.clone(),
        secret_key: body.secret_key.clone(),
        region: body
            .region
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("us-east-1")
            .to_string(),
    };

    let s3 = match S3Store::new(&cfg).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Details go to the server log only, so the login form cannot be used
            // to probe what is listening behind an endpoint.
            tracing::warn!("login to {endpoint} bucket {} failed: {e:#}", cfg.bucket);
            return err(
                StatusCode::BAD_GATEWAY,
                "could not open the bucket: check the endpoint, bucket name and credentials",
            );
        }
    };

    tracing::info!("new session for {endpoint} bucket {}", cfg.bucket);
    let token = state.sessions.create(s3, master_key);
    HttpResponse::Ok().json(json!({ "token": token, "encrypted": master_key.is_some() }))
}

fn parse_master_key(input: Option<&str>) -> Result<Option<[u8; 32]>, &'static str> {
    let s = match input.map(str::trim) {
        None | Some("") => return Ok(None),
        Some(s) => s,
    };
    let bytes = hex::decode(s)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(s))
        .map_err(|_| "invalid master key encoding (use hex or base64)")?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "master key must be exactly 32 bytes")?;
    Ok(Some(key))
}

pub async fn logout(req: HttpRequest, state: web::Data<AppState>) -> impl Responder {
    if let Some(token) = req
        .headers()
        .get("x-dfs-token")
        .and_then(|v| v.to_str().ok())
    {
        state.sessions.remove(token);
    }
    HttpResponse::Ok().json(json!({ "ok": true }))
}

// ── Listing and metadata ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
    /// `inline=1` asks for a preview-friendly response instead of an attachment.
    pub inline: Option<String>,
}

fn required_path(q: &PathQuery) -> Result<&str, HttpResponse> {
    q.path
        .as_deref()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "path required"))
}

fn lookup<'a>(snap: &'a ParsedSnapshot, path: &str) -> Result<(Uuid, &'a InodeMeta), HttpResponse> {
    let id = snap
        .resolve(path)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "path not found"))?;
    let inode = snap
        .inodes
        .get(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "inode not found"))?;
    Ok((id, inode))
}

pub async fn ls(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PathQuery>,
) -> HttpResponse {
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let path = query.path.as_deref().unwrap_or("/");
    match snap.list_dir(path) {
        Some(entries) => HttpResponse::Ok().json(json!({ "path": path, "entries": entries })),
        None => err(StatusCode::NOT_FOUND, "path not found"),
    }
}

#[derive(Serialize)]
struct ShardInfo<'a> {
    index: u8,
    endpoint: &'a str,
    s3_key: &'a str,
}

#[derive(Serialize)]
struct ChunkInfo<'a> {
    chunk_id: Uuid,
    offset: u64,
    length: u32,
    sha256: String,
    encrypted: bool,
    /// "s3", or "local" for chunks that never left a device's cache.
    storage: &'static str,
    ec_k: u8,
    ec_m: u8,
    shards: Vec<ShardInfo<'a>>,
}

fn describe_chunk(c: &ChunkRef) -> ChunkInfo<'_> {
    let (storage, ec_k, ec_m, shards) = match &c.location {
        ChunkLocation::Local { .. } => ("local", 1, 0, Vec::new()),
        ChunkLocation::Distributed { shards, ec_k, ec_m } => (
            "s3",
            *ec_k,
            *ec_m,
            shards
                .iter()
                .map(|s: &ShardLocation| ShardInfo {
                    index: s.shard_index,
                    endpoint: &s.endpoint,
                    s3_key: &s.s3_key,
                })
                .collect(),
        ),
    };
    ChunkInfo {
        chunk_id: c.chunk_id,
        offset: c.offset,
        length: c.length,
        sha256: hex::encode(c.checksum),
        encrypted: c.encrypted,
        storage,
        ec_k,
        ec_m,
        shards,
    }
}

fn version_vector_json(vv: &VersionVector) -> BTreeMap<String, u64> {
    vv.clocks.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// Everything DFS knows about one inode.
pub async fn stat(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PathQuery>,
) -> HttpResponse {
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let path = match required_path(&query) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (id, inode) = match lookup(&snap, path) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let chunks: Vec<ChunkInfo> = inode
        .chunks
        .iter()
        .take(MAX_STAT_CHUNKS)
        .map(describe_chunk)
        .collect();
    let mut xattrs: Vec<&String> = inode.xattrs.keys().collect();
    xattrs.sort();

    HttpResponse::Ok().json(json!({
        "path": path,
        "id": id,
        "kind": inode.kind,
        "size": inode.size,
        "mode": format!("{:o}", inode.mode),
        "uid": inode.uid,
        "gid": inode.gid,
        "nlink": inode.nlink,
        "mtime_ms": system_time_to_ms(inode.mtime),
        "atime_ms": system_time_to_ms(inode.atime),
        "ctime_ms": system_time_to_ms(inode.ctime),
        "symlink_target": inode.symlink_target,
        "xattrs": xattrs,
        "version_vector": version_vector_json(&inode.version_vector),
        "devices": short_device_ids(&inode.version_vector),
        "chunk_count": inode.chunks.len(),
        "chunks": chunks,
        "can_decrypt": sess.master_key.is_some(),
    }))
}

/// Saved versions of a file (`meta/versions/<device>/<file>/<seq>.msgpack`).
pub async fn versions(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PathQuery>,
) -> HttpResponse {
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let path = match required_path(&query) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (file_id, _) = match lookup(&snap, path) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let mut keys = Vec::new();
    for dev in &snap.devices {
        let prefix = format!("meta/versions/{}/{}/", dev.device_id, file_id);
        match sess.s3.list_objects_with_prefix(&prefix).await {
            Ok(k) => keys.extend(k.into_iter().map(|k| (dev.device_id, k))),
            Err(e) => tracing::warn!("listing {prefix}: {e:#}"),
        }
    }
    let total = keys.len();

    let mut out = Vec::new();
    for (device_id, key) in keys {
        let seq: u64 = key
            .rsplit('/')
            .next()
            .and_then(|f| f.trim_end_matches(".msgpack").parse().ok())
            .unwrap_or(0);
        let entry = match sess
            .s3
            .get_object(&key)
            .await
            .and_then(|d| rmp_serde::from_slice::<InodeMeta>(&d).map_err(anyhow::Error::from))
        {
            Ok(inode) => json!({
                "device": device_id.to_string(),
                "seq": seq,
                "key": key,
                "size": inode.size,
                "mtime_ms": system_time_to_ms(inode.mtime),
                "chunks": inode.chunks.len(),
                "encrypted": inode.chunks.iter().any(|c| c.encrypted),
            }),
            Err(e) => {
                json!({ "device": device_id.to_string(), "seq": seq, "key": key, "error": format!("{e:#}") })
            }
        };
        out.push(entry);
    }
    out.sort_by_key(|v| std::cmp::Reverse(v["mtime_ms"].as_u64().unwrap_or(0)));
    out.truncate(MAX_VERSIONS);

    HttpResponse::Ok().json(json!({ "file_id": file_id, "total": total, "versions": out }))
}

/// Volume-wide numbers: devices, inode counts, bytes, chunk layout.
pub async fn stats(req: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };

    let (mut files, mut dirs, mut symlinks, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let (mut chunk_count, mut encrypted_chunks, mut local_chunks) = (0u64, 0u64, 0u64);
    let mut layouts: BTreeMap<String, u64> = BTreeMap::new();
    let mut writes_per_device: HashMap<String, u64> = HashMap::new();

    for inode in snap.inodes.values() {
        match inode.kind {
            InodeKind::File => files += 1,
            InodeKind::Directory => dirs += 1,
            InodeKind::Symlink => symlinks += 1,
        }
        if inode.kind == InodeKind::File {
            bytes += inode.size;
        }
        for dev in short_device_ids(&inode.version_vector) {
            *writes_per_device.entry(dev).or_default() += 1;
        }
        for c in &inode.chunks {
            chunk_count += 1;
            encrypted_chunks += c.encrypted as u64;
            match &c.location {
                ChunkLocation::Local { .. } => local_chunks += 1,
                ChunkLocation::Distributed { ec_k, ec_m, .. } => {
                    *layouts.entry(format!("{ec_k}+{ec_m}")).or_default() += 1;
                }
            }
        }
    }

    // Largest files, with their full paths.
    let mut paths: HashMap<Uuid, String> = HashMap::new();
    let mut stack = vec![(Uuid::nil(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for (name, child) in snap.children.get(&dir).into_iter().flatten() {
            let p = format!("{prefix}/{name}");
            if paths.insert(*child, p.clone()).is_none()
                && snap
                    .inodes
                    .get(child)
                    .is_some_and(|i| i.kind == InodeKind::Directory)
            {
                stack.push((*child, p));
            }
        }
    }
    let mut largest: Vec<(&InodeMeta, &String)> = snap
        .inodes
        .values()
        .filter(|i| i.kind == InodeKind::File)
        .filter_map(|i| paths.get(&i.id).map(|p| (i, p)))
        .collect();
    largest.sort_by_key(|(i, _)| std::cmp::Reverse(i.size));
    let largest: Vec<_> = largest
        .into_iter()
        .take(8)
        .map(|(i, p)| json!({ "path": p, "size": i.size, "chunks": i.chunks.len() }))
        .collect();

    let devices: Vec<_> = snap
        .devices
        .iter()
        .map(|d| {
            let short = d.device_id.to_string()[..8].to_string();
            json!({
                "device_id": d.device_id,
                "short": short,
                "snapshot_ms": d.snapshot_ms,
                "snapshot_bytes": d.snapshot_bytes,
                "inodes": d.inodes,
                "dentries": d.dentries,
                "inodes_written": writes_per_device.get(&short).copied().unwrap_or(0),
            })
        })
        .collect();

    HttpResponse::Ok().json(json!({
        "endpoint": sess.s3.endpoint,
        "bucket": sess.s3.bucket,
        "files": files,
        "directories": dirs,
        "symlinks": symlinks,
        "bytes": bytes,
        "chunks": chunk_count,
        "encrypted_chunks": encrypted_chunks,
        "local_only_chunks": local_chunks,
        "layouts": layouts,
        "devices": devices,
        "largest": largest,
    }))
}

// ── File content ──────────────────────────────────────────────────────────────

/// Quote-safe filename for Content-Disposition, plus an RFC 5987 UTF-8 variant.
fn content_disposition(kind: &str, name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

pub async fn download(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PathQuery>,
) -> HttpResponse {
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let path = match required_path(&query) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (_, inode) = match lookup(&snap, path) {
        Ok(v) => v,
        Err(r) => return r,
    };

    if inode.kind != InodeKind::File {
        return err(StatusCode::BAD_REQUEST, "not a file");
    }
    if inode.size > state.settings.max_file_bytes {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file is larger than DFS_BROWSER_MAX_FILE_MB",
        );
    }
    if inode.chunks.iter().any(|c| c.encrypted) && sess.master_key.is_none() {
        return err(
            StatusCode::FORBIDDEN,
            "file is encrypted: log in with the master key to read it",
        );
    }

    let data = match chunks::read_file(inode, &sess.s3, sess.master_key, &state.settings).await {
        Ok(d) => d,
        Err(e) => return internal("reading file", e),
    };

    let filename = path.rsplit('/').next().unwrap_or("file");
    let mime = mime_guess::from_path(filename).first_or_octet_stream();
    let inline = query
        .inline
        .as_deref()
        .is_some_and(|v| v == "1" || v == "true");

    HttpResponse::Ok()
        .insert_header((
            "Content-Disposition",
            content_disposition(if inline { "inline" } else { "attachment" }, filename),
        ))
        // Never let stored content run as a page on this origin.
        .insert_header(("Content-Security-Policy", "sandbox; default-src 'none'"))
        .insert_header(("Cache-Control", "private, no-store"))
        .content_type(mime.as_ref())
        .body(data)
}

pub async fn upload(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<PathQuery>,
    body: web::Bytes,
) -> HttpResponse {
    if state.settings.read_only {
        return err(StatusCode::FORBIDDEN, "this server is read-only");
    }
    let sess = match session(&req, &state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let path = match required_path(&query) {
        Ok(p) => p,
        Err(r) => return r,
    };

    let (parent_path, filename) = match path.rfind('/') {
        Some(i) => (&path[..i.max(1)], &path[i + 1..]),
        None => ("/", path),
    };
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.chars().any(char::is_control)
    {
        return err(StatusCode::BAD_REQUEST, "invalid file name");
    }

    let snap = match snapshot(&sess).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let parent_id = match snap.resolve(parent_path) {
        Some(id) => id,
        None => {
            return err(
                StatusCode::NOT_FOUND,
                format!("parent directory '{parent_path}' not found"),
            )
        }
    };
    if !parent_id.is_nil()
        && snap
            .inodes
            .get(&parent_id)
            .is_some_and(|i| i.kind != InodeKind::Directory)
    {
        return err(StatusCode::BAD_REQUEST, "parent is not a directory");
    }
    if snap
        .children
        .get(&parent_id)
        .is_some_and(|c| c.iter().any(|(n, _)| n == filename))
    {
        return err(
            StatusCode::CONFLICT,
            format!("'{filename}' already exists in this folder"),
        );
    }

    let chunk_data = body.to_vec();
    let file_size = chunk_data.len() as u64;
    let chunk_id = Uuid::now_v7();
    let checksum: [u8; 32] = Sha256::digest(&chunk_data).into();

    // Same layout the daemon uses without a coordinator: one chunk, one replica.
    let (stored, encrypted) = match sess.master_key {
        Some(ref key) => match dfs_daemon::store::crypto::encrypt(chunk_id, &chunk_data, key) {
            Ok(enc) => (enc, true),
            Err(e) => return internal("encrypt", e),
        },
        None => (chunk_data, false),
    };

    let s3_key = match sess.s3.put_chunk(chunk_id, &stored).await {
        Ok(k) => k,
        Err(e) => return internal("upload", e),
    };

    let now = SystemTime::now();
    let file_id = Uuid::now_v7();
    let mut vv = VersionVector::new();
    vv.increment(sess.browser_device_id);
    let inode = InodeMeta {
        id: file_id,
        kind: InodeKind::File,
        size: file_size,
        atime: now,
        mtime: now,
        ctime: now,
        mode: 0o644,
        uid: 0,
        gid: 0,
        nlink: 1,
        chunks: vec![ChunkRef {
            chunk_id,
            offset: 0,
            length: file_size as u32,
            checksum,
            location: ChunkLocation::Distributed {
                shards: vec![ShardLocation {
                    node_id: Uuid::nil(),
                    endpoint: sess.s3.endpoint.clone(),
                    shard_index: 0,
                    s3_key: s3_key.clone(),
                }],
                ec_k: 1,
                ec_m: 0,
            },
            encrypted,
        }],
        version_vector: vv,
        tombstone: false,
        tombstone_time: None,
        symlink_target: None,
        xattrs: HashMap::new(),
    };

    if let Err(e) = write_file_to_browser_snapshot(
        &sess.s3,
        sess.browser_device_id,
        inode,
        parent_id,
        filename.to_string(),
    )
    .await
    {
        return internal("metadata write", e);
    }

    // Invalidate the cached tree so the next listing shows the new file.
    *sess.cache.write().await = None;
    tracing::info!("uploaded {path} ({file_size} bytes, encrypted={encrypted})");

    HttpResponse::Ok().json(json!({
        "ok": true,
        "path": path,
        "file_id": file_id,
        "chunk_id": chunk_id,
        "s3_key": s3_key,
        "encrypted": encrypted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_key_parsing() {
        assert_eq!(parse_master_key(None).unwrap(), None);
        assert_eq!(parse_master_key(Some("  ")).unwrap(), None);
        assert!(parse_master_key(Some(&"ab".repeat(32))).unwrap().is_some());
        assert!(parse_master_key(Some("abcd")).is_err());
        let b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(parse_master_key(Some(&b64)).unwrap(), Some([7u8; 32]));
    }

    #[test]
    fn disposition_is_quote_safe() {
        let h = content_disposition("attachment", "a\"b\nc.txt");
        assert!(h.starts_with("attachment; filename=\"a_b_c.txt\""));
        assert!(h.contains("filename*=UTF-8''a%22b%0Ac.txt"));
    }
}
