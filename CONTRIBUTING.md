# Contributing

Thanks for taking the time to improve DFS Browser.

## Development setup

You need Rust (stable) and the FUSE headers that the `dfs-daemon` crate compiles
against (`libfuse3-dev` and `pkg-config` on Debian/Ubuntu). The browser never mounts
anything; the headers are only needed to build.

```bash
cargo build
cargo test
cargo clippy --all-targets
```

For a data set to click through, start the demo MinIO and seed it:

```bash
docker compose --env-file demo/demo.env --profile demo up -d --build
# or run the seeder against any S3 bucket you own:
DFS_DEMO_ENDPOINT=http://localhost:9000 DFS_DEMO_ACCESS_KEY=... DFS_DEMO_SECRET_KEY=... \
  cargo run --bin dfs-browser-demo-seed
cargo run   # http://127.0.0.1:8888
```

The UI is a single file, `src/ui.html`, with no build step. It is compiled into the
binary, so restart `cargo run` after editing it.

## Pull requests

- Keep changes focused; one topic per pull request.
- Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test` before pushing.
- Add an entry under *Unreleased* in `CHANGELOG.md` for user-visible changes.
- The browser reads the on-S3 formats defined by [DFS](https://github.com/mmiretta/dfs).
  If a change depends on a newer DFS revision, bump the pinned `rev` in `Cargo.toml`
  and mention it in the pull request.
- Never include real credentials, bucket names or hostnames in code, tests,
  screenshots or issues. The demo data is synthetic on purpose.

## Reporting security issues

Please do not open a public issue for a vulnerability. Use GitHub's private
"Report a vulnerability" form on the repository instead.
