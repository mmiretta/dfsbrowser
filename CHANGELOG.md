# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-21

First public release.

### Added
- Browse a DFS volume straight from its S3 bucket: device snapshots under
  `meta/snapshots/` are merged into one directory tree.
- File details: inode metadata, version vector per device, and every chunk with its
  offset, length, SHA-256, encryption flag and Reed-Solomon shard layout.
- Saved versions from `meta/versions/`, newest first.
- Inline previews for text and image files (SVG is rendered through `<img>`).
- Volume view: file/folder/chunk counts, logical size, encryption ratio, chunk layouts,
  device snapshots and the largest files.
- Downloads of plain, AES-256-GCM encrypted and erasure-coded files.
- Uploads (single chunk, encrypted when the session has a master key), written as a
  browser device snapshot plus a `FileCreate` delta so running daemons pick them up.
- Optional HTTP Basic authentication (`DFS_BROWSER_AUTH_USER` / `DFS_BROWSER_AUTH_PASSWORD`).
- S3 endpoint allowlist (`DFS_BROWSER_ALLOWED_ENDPOINTS`), also applied to shard endpoints.
- Read-only mode, session lifetime and file size limits.
- Docker image (distroless, non-root) and a Compose `demo` profile with a throwaway
  MinIO seeded with a synthetic DFS volume.

### Security
- Binds to `127.0.0.1:8888` by default.
- Session tokens travel only in the `x-dfs-token` header (no cookies) and expire.
- Login errors no longer echo upstream connection details back to the browser.
- File names are HTML-escaped in the UI; downloads are served with
  `Content-Security-Policy: sandbox`; the UI sends a strict CSP and `X-Frame-Options: DENY`.
- The no-op delete and mkdir endpoints were removed.
