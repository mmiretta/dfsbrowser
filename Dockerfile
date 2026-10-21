# syntax=docker/dockerfile:1

# ── Build ─────────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

# dfs-daemon pulls in the FUSE bindings, which need these headers to compile.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libfuse3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Build dependencies first so source edits don't invalidate that layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin && echo 'fn main() {}' > src/main.rs && echo 'fn main() {}' > src/bin/demo_seed.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/dfs_browser* target/release/.fingerprint/dfs-browser-*

COPY src ./src
COPY demo ./demo
RUN cargo build --release --locked

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/dfs-browser /usr/local/bin/dfs-browser
COPY --from=builder /build/target/release/dfs-browser-demo-seed /usr/local/bin/dfs-browser-demo-seed

# Inside the container listen on all interfaces; docker-compose.yml publishes the
# port on 127.0.0.1 only. The binary itself defaults to 127.0.0.1:8888.
ENV DFS_BROWSER_BIND=0.0.0.0:8888
EXPOSE 8888
USER nonroot
ENTRYPOINT ["/usr/local/bin/dfs-browser"]
