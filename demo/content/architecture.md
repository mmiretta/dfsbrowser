# Home lab architecture

Three MinIO nodes hold the DFS bucket. New chunks are split into **4 data + 2 parity**
shards, so any two nodes can be offline without losing data.

| Node    | Role                  | Capacity |
|---------|-----------------------|----------|
| node-a  | MinIO, coordinator    | 4 TB     |
| node-b  | MinIO                 | 4 TB     |
| node-c  | MinIO, cloud mirror   | 8 TB     |

## Devices

- **laptop**: daily work, mounts `/dfs`
- **desktop**: builds and photo editing
- **nas**: backups and media, always on

## Encryption

The `private/` folder is written with a master key, so its chunks are AES-256-GCM
encrypted before they reach S3. Without the key the browser can list the files but
cannot open them.

## Versions

Every flush saves the previous inode under `meta/versions/`, so older copies of a file
can be inspected and restored.
