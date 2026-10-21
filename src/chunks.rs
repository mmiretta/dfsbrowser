//! Reading file content back from S3: plain chunks, erasure-coded shards and
//! AES-256-GCM encrypted chunks.

use crate::config::{normalize_endpoint, Settings};
use dfs_daemon::fs::inode::{ChunkLocation, ChunkRef, InodeMeta};
use dfs_daemon::store::s3::S3Store;
use dfs_daemon::store::{crypto, erasure};

/// AES-256-GCM appends a 16-byte authentication tag to every encrypted chunk.
const GCM_TAG_LEN: usize = 16;

/// Assemble the full plaintext of a file.
pub async fn read_file(
    inode: &InodeMeta,
    s3: &S3Store,
    master_key: Option<[u8; 32]>,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    let mut data = vec![0u8; inode.size as usize];
    for chunk in &inode.chunks {
        let plaintext = read_chunk(chunk, s3, master_key, settings).await?;
        let start = (chunk.offset as usize).min(data.len());
        let end = (start + chunk.length as usize).min(data.len());
        let copy = (end - start).min(plaintext.len());
        data[start..start + copy].copy_from_slice(&plaintext[..copy]);
    }
    Ok(data)
}

async fn read_chunk(
    chunk: &ChunkRef,
    s3: &S3Store,
    master_key: Option<[u8; 32]>,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    let raw = fetch_chunk_bytes(chunk, s3, settings).await?;
    if !chunk.encrypted {
        return Ok(raw);
    }
    let key = master_key
        .ok_or_else(|| anyhow::anyhow!("file is encrypted but no master key was provided"))?;
    crypto::decrypt(chunk.chunk_id, &raw, &key)
}

/// Fetch the stored bytes of one chunk (ciphertext if the chunk is encrypted).
async fn fetch_chunk_bytes(
    chunk: &ChunkRef,
    s3: &S3Store,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    let (shards, k, m) = match &chunk.location {
        // Only present in a daemon's local cache; nothing to fetch from S3.
        ChunkLocation::Local { .. } => {
            anyhow::bail!(
                "chunk {} only exists in a device's local cache",
                chunk.chunk_id
            )
        }
        ChunkLocation::Distributed { shards, ec_k, ec_m } => {
            (shards, *ec_k as usize, *ec_m as usize)
        }
    };
    if shards.is_empty() {
        return Ok(Vec::new());
    }
    if m == 0 && k <= 1 {
        return s3.get_chunk(&shards[0].s3_key).await;
    }

    // Erasure-coded: each shard may live on a different S3 node.
    let mut opts: Vec<Option<Vec<u8>>> = vec![None; k + m];
    let mut present = 0;
    for shard in shards {
        let idx = shard.shard_index as usize;
        if idx >= opts.len() || present >= k {
            continue;
        }
        let data = if shard.endpoint.is_empty()
            || normalize_endpoint(&shard.endpoint) == normalize_endpoint(&s3.endpoint)
        {
            s3.get_shard(&shard.s3_key).await
        } else if settings.endpoint_allowed(&shard.endpoint) {
            match s3.for_endpoint(&shard.endpoint).await {
                Ok(node) => node.get_shard(&shard.s3_key).await,
                Err(e) => Err(e),
            }
        } else {
            tracing::warn!("skipping shard on non-allowed endpoint {}", shard.endpoint);
            continue;
        };
        match data {
            Ok(d) => {
                opts[idx] = Some(d);
                present += 1;
            }
            Err(e) => tracing::warn!("shard {}: {e:#}", shard.s3_key),
        }
    }

    // Shards hold the stored bytes, which for encrypted chunks include the tag.
    let stored_len = chunk.length as usize + if chunk.encrypted { GCM_TAG_LEN } else { 0 };
    erasure::decode(opts, k, m, stored_len)
}
