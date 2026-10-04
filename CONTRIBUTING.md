# Contributing to grpc-ffmpeg

Bug reports, ideas and pull requests are welcome.

- **Bugs:** open an [issue](https://github.com/CrystalNET-org/grpc-ffmpeg/issues) with the
  steps to reproduce, what you expected and what happened, and the versions of the worker
  image, the client and Jellyfin (if involved). The worker's log and the client's activity
  log (`LOG_FILE`) usually show what went wrong.
- **Ideas:** open an issue describing the problem it solves before writing larger changes.
- **Pull requests:** against `main`. Keep them focused, and update the README when behaviour
  or configuration changes.

## Repository layout

```
grpc-ffmpeg/
├── src/
│   ├── proto/ffmpeg.proto       # gRPC API shared by worker and clients
│   ├── server/                  # worker (Python) and the health check sample video
│   └── client/rust/             # client
├── docker/Dockerfile.server     # worker image
├── tests/                       # worker unit tests
├── example_deployment/          # Kubernetes and docker compose examples
├── scripts/next-release-tag.sh  # computes the next release tag
├── .woodpecker/                 # CI pipelines
├── renovate.json                # dependency updates
├── requirements.txt             # Python runtime dependencies
└── requirements-build.txt       # Python stub generation (grpcio-tools)
```

## Worker

The worker is a Python asyncio gRPC server. It needs Python 3.10 or later and an ffmpeg
installation, ideally jellyfin-ffmpeg.

```bash
python3 -m venv venv && . venv/bin/activate
pip install -r requirements.txt -r requirements-build.txt

# Generate the gRPC stubs
mkdir -p gen
python -m grpc_tools.protoc -I src/proto --python_out=gen --grpc_python_out=gen src/proto/ffmpeg.proto

# Run it against a local ffmpeg
PYTHONPATH=gen VALID_TOKEN=secret BINARY_PATH_PREFIX=/usr/lib/jellyfin-ffmpeg/ \
  python src/server/grpc-ffmpeg.py
```

Unit tests (with the stubs generated as above):

```bash
PYTHONPATH=gen python -m unittest discover tests
```

The worker image bundles jellyfin-ffmpeg and the VAAPI drivers:

```bash
docker build -f docker/Dockerfile.server -t ffmpeg-worker .
```

[`example_deployment/compose/docker-compose.yml`](example_deployment/compose/docker-compose.yml)
builds and starts a worker from the source; test it with a locally built client.

## Client

The client is in `src/client/rust`. It needs a Rust toolchain and `protoc`
(the build generates the gRPC code from `src/proto/ffmpeg.proto`).

```bash
cd src/client/rust
cargo test
cargo build --release
ln -sf "$PWD/target/release/grpc-ffmpeg-client" /tmp/ffmpeg
GRPC_HOST=localhost AUTH_TOKEN=secret /tmp/ffmpeg -version
```

The release binaries are fully static. They are built like this (see
[`.cargo/config.toml`](src/client/rust/.cargo/config.toml)):

```bash
# Linux amd64 and arm64 (musl); needs clang and llvm
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl

# Windows amd64; needs cargo-zigbuild and zig (pip install ziglang cargo-zigbuild)
rustup target add x86_64-pc-windows-gnu
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

Cargo uses one job per CPU core, at about 150 MB each. On machines with many cores and little
memory, limit it with `CARGO_BUILD_JOBS`.

## Code style

- Python: PEP 8, matching the existing code.
- Rust: match the existing code; `cargo test` must pass.
- Changes to `ffmpeg.proto` must stay compatible: clients and workers of different versions
  have to keep working together. Add fields and methods, don't change or remove them.

## CI

The pipelines in `.woodpecker/` run on [Woodpecker CI](https://woodpecker-ci.org/):

| Pipeline | Runs on | Does |
| --- | --- | --- |
| `build_pr.yaml` | pull requests | Test-builds the worker image |
| `build_dev_version.yaml` | pushes to `main` | Builds and pushes the worker image as `dev` |
| `build_rust_client.yaml` | pushes to `main` | Runs the client tests and builds all client binaries |
| `auto_release.yaml` | pushes to `main` that change the worker Dockerfile | Tags a release, after the two builds above succeeded |
| `build_tag_version.yaml` | tags | Builds and pushes the worker image with the tag |
| `build_tag_version_rust_client.yaml` | tags | Builds the client binaries and attaches them to the GitHub release |
| `renovate.yaml` | cron, manual | Runs Renovate |

## Releases

Release tags are `<jellyfin-ffmpeg version>-<major>.<minor>`, e.g. `8.1.3-7.8` for
jellyfin-ffmpeg `8.1.3-1`. The minor number increases with every release.

New jellyfin-ffmpeg versions are released automatically:

1. Renovate opens a PR that updates `JELLYFIN_FFMPEG_VERSION` in `docker/Dockerfile.server`,
   6 hours after the upstream release. It stays on the current major version; a new major
   version goes with the Jellyfin release that uses it and is updated by hand.
2. The PR pipeline test-builds the worker image, and Renovate merges the PR once it passes.
3. On `main`, once the builds succeeded, `auto_release.yaml` pushes the next tag
   (`scripts/next-release-tag.sh`), which publishes the worker image and the client binaries.

Other changes are released by pushing the next tag by hand. The Jellyfin plugin picks up new
releases automatically through its own Renovate setup.
