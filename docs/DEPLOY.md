# Deploying DFS Browser

This guide covers a production-style setup: the container on a host that can reach
your DFS bucket, a reverse proxy in front for TLS, and upgrades.

## 1. Configure

```bash
git clone https://github.com/mmiretta/dfsbrowser.git
cd dfsbrowser
cp .env.example .env
```

Edit `.env`. A typical setup:

```dotenv
DFS_BROWSER_ALLOWED_ENDPOINTS=https://s3.example.com
DFS_BROWSER_DEFAULT_ENDPOINT=https://s3.example.com
DFS_BROWSER_DEFAULT_BUCKET=dfs
DFS_BROWSER_AUTH_USER=admin
DFS_BROWSER_AUTH_PASSWORD=<long random string>
DFS_BROWSER_READ_ONLY=true
```

Generate a password with `openssl rand -base64 24`. Skip the Basic auth variables only
if the proxy already authenticates users (SSO, OAuth2 proxy, client certificates) or
the browser is reachable from `localhost` alone.

If the S3 endpoint is another container on the same Docker host, attach
`dfs-browser` to that container's network (see *Networking* below) and use its service
name, e.g. `http://minio:9000`.

## 2. Start

```bash
docker compose up -d --build
docker compose logs -f dfs-browser
curl -s http://127.0.0.1:8888/healthz
```

The service is published on `127.0.0.1:8888` (change with `DFS_BROWSER_HOST_BIND` and
`DFS_BROWSER_PORT`). The container runs as a non-root user with a read-only root
filesystem and no capabilities; it needs no volumes because sessions live in memory.

The startup log warns when Basic auth or the endpoint allowlist is not set.

## 3. Reverse proxy with TLS

Put the proxy on the same host and point it at `127.0.0.1:8888`. The app does not need
WebSockets or sticky sessions (run a single replica: sessions are per process).

Uploads are sent as one request body, so raise the proxy's body limit to match
`DFS_BROWSER_MAX_FILE_MB`.

### Caddy

```caddyfile
dfs.example.com {
    encode zstd gzip
    request_body {
        max_size 512MB
    }
    header {
        Strict-Transport-Security "max-age=31536000"
    }
    reverse_proxy 127.0.0.1:8888
}
```

Caddy obtains and renews the certificate automatically. To let Caddy handle the
password instead of the app, leave the `DFS_BROWSER_AUTH_*` variables empty and add:

```caddyfile
    basic_auth {
        admin <hash from `caddy hash-password`>
    }
```

### nginx

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name dfs.example.com;

    ssl_certificate     /etc/letsencrypt/live/dfs.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/dfs.example.com/privkey.pem;
    add_header Strict-Transport-Security "max-age=31536000" always;

    client_max_body_size 512m;
    proxy_read_timeout   300s;   # large erasure-coded downloads are assembled first

    location / {
        proxy_pass         http://127.0.0.1:8888;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;
        proxy_buffering    off;
    }
}

server {
    listen 80;
    server_name dfs.example.com;
    return 301 https://$host$request_uri;
}
```

### Proxy in a container

When the proxy runs in Docker on the same host, `127.0.0.1` inside its container is
not the host. Either put both containers on a shared network and proxy to
`dfs-browser:8888`, or publish the app on the host's Docker bridge gateway address
instead of loopback (`DFS_BROWSER_HOST_BIND=<bridge gateway>`, see
`docker network inspect bridge`). Do not publish it on
`0.0.0.0` unless a firewall restricts the port.

## Networking

A shared network with an existing MinIO or proxy can be added with a
`docker-compose.override.yml`, which Compose picks up automatically:

```yaml
services:
  dfs-browser:
    networks: [default, storage]
networks:
  storage:
    external: true
    name: my-minio-network
```

## Upgrades

```bash
git pull
docker compose up -d --build
docker image prune -f
```

Restarting ends all sessions; users log in again. Read [CHANGELOG.md](../CHANGELOG.md)
before upgrading across minor versions. The pinned DFS revision in `Cargo.toml`
decides which on-S3 format the browser understands; upgrade the browser together with
your DFS devices when the DFS format changes.

To pin a release instead of following `main`, check out a tag (`git checkout v0.1.0`)
and set `DFS_BROWSER_TAG=0.1.0` in `.env` so the image is tagged accordingly.

## Running without Docker

```bash
sudo apt install pkg-config libfuse3-dev
cargo build --release
sudo install -m 755 target/release/dfs-browser /usr/local/bin/
```

A minimal systemd unit:

```ini
[Unit]
Description=DFS Browser
After=network-online.target

[Service]
EnvironmentFile=/etc/dfs-browser.env
ExecStart=/usr/local/bin/dfs-browser
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Put the same variables as in `.env` into `/etc/dfs-browser.env` (mode `600`).
