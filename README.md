# GRPC FFmpeg Service

[![GPL3](https://img.shields.io/badge/license-GPLv3-blue)](#) [![last release](https://img.shields.io/github/v/release/CrystalNET-org/grpc-ffmpeg)](https://github.com/CrystalNET-org/grpc-ffmpeg/releases) [![Pipeline Status](https://ci.cluster.lan.crystalnet.org/api/badges/10/status.svg)](https://ci.cluster.lan.crystalnet.org/repos/10) [![Discord](https://dcbadge.limes.pink/api/server/Yj5AYwcGXu?style=flat)](https://discord.gg/Yj5AYwcGXu)

## Overview

grpc-ffmpeg runs `ffmpeg` and `ffprobe` on remote worker machines, for example to offload
Jellyfin transcoding to nodes with a GPU. It is inspired by [rFFmpeg](https://github.com/joshuaboniface/rffmpeg),
but uses gRPC instead of SSH.

- **Worker** (server): a container with [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg)
  and Intel/NVIDIA VAAPI drivers that executes the commands it receives.
- **Client**: a small static binary that you install as `ffmpeg` and `ffprobe`. It forwards
  its arguments to a worker and relays the results.

```
Jellyfin ──runs──▶ ffmpeg (client) ══gRPC══▶ worker ──runs──▶ jellyfin-ffmpeg
         ◀─ stdout/stderr/exit code ════════════════ ◀─
```

The client behaves like a local ffmpeg:

- stdout and stderr are relayed byte for byte, in real time (including progress output)
- the exit code is passed through, including death by a signal
- stdin is forwarded, so Jellyfin's `q` stops a remote transcode gracefully
- arguments reach the worker exactly as given, however they are quoted

The worker executes commands, but files are not transferred. **Media, transcode and cache
directories must be mounted at the same paths on Jellyfin and on every worker** (e.g. over NFS
or a shared volume).

## Quick start with Jellyfin

### 1. Run a worker

Images are published to `ghcr.io/crystalnet-org/ffmpeg-worker` (amd64). Use a release tag, or
`dev` for the latest build of `main`.

```bash
docker run -d --name ffmpeg-worker \
  -p 50051:50051 -p 8080:8080 \
  -e VALID_TOKEN=change-me \
  --device /dev/dri:/dev/dri \
  -v /srv/media:/media \
  -v /srv/jellyfin/cache:/cache \
  ghcr.io/crystalnet-org/ffmpeg-worker:dev
```

Mount the same paths that Jellyfin uses. For Kubernetes, see
[`example_deployment/kubernetes`](example_deployment/kubernetes) (a DaemonSet with one worker
per node and a Service named `ffmpeg-workers`). There is also a
[docker compose example](example_deployment/compose/docker-compose.yml).

### 2. Install the client in the Jellyfin container

Download the static client for your platform (`grpc-ffmpeg-client-amd64`,
`grpc-ffmpeg-client-arm64` or `grpc-ffmpeg-client-windows-amd64.exe`) from the
[latest release](https://github.com/CrystalNET-org/grpc-ffmpeg/releases/latest). Install it
under the names `ffmpeg` and `ffprobe` in the same directory; the client runs the binary
whose name it was invoked as.

```bash
mkdir -p /opt/grpc-ffmpeg
curl -L -o /opt/grpc-ffmpeg/grpc-ffmpeg-client \
  https://github.com/CrystalNET-org/grpc-ffmpeg/releases/latest/download/grpc-ffmpeg-client-amd64
chmod +x /opt/grpc-ffmpeg/grpc-ffmpeg-client
ln -s grpc-ffmpeg-client /opt/grpc-ffmpeg/ffmpeg
ln -s grpc-ffmpeg-client /opt/grpc-ffmpeg/ffprobe
```

Configure the client with a `grpc-ffmpeg.conf` next to it (or environment variables, see
[Configuration](#client)):

```ini
GRPC_HOST=ffmpeg-workers        # worker host name, Service or load balancer
AUTH_TOKEN=change-me            # must match the worker's VALID_TOKEN
FALLBACK_DIR=/usr/lib/jellyfin-ffmpeg
RETRIES=2
```

Jellyfin refuses to start if its ffmpeg check fails, so setting `FALLBACK_DIR` to a local
ffmpeg installation is recommended.

On Windows, copy `grpc-ffmpeg-client-windows-amd64.exe` to `ffmpeg.exe` and `ffprobe.exe` in
the same directory instead of creating symlinks.

### 3. Point Jellyfin at the client

Set the FFmpeg path to `/opt/grpc-ffmpeg/ffmpeg`. You can do this under *Dashboard → Playback
→ Transcoding*, or with the `JELLYFIN_FFMPEG` environment variable. Check that it works:

```bash
/opt/grpc-ffmpeg/ffmpeg -version
```

## Configuration

### Worker

| Variable | Default | Description |
| --- | --- | --- |
| `VALID_TOKEN` | *(unset)* | Token clients must send. When set, calls without it are rejected with `UNAUTHENTICATED`. When unset, authentication is disabled and a warning is logged. |
| `BINARY_PATH_PREFIX` | `/usr/lib/jellyfin-ffmpeg/` | Directory containing the binaries. Only `ffmpeg`, `ffprobe`, `mediainfo` and `vainfo` can be executed. |
| `MAX_FFMPEG_WORKERS` | `10` | Maximum number of concurrent `ffmpeg` processes (`0` = unlimited). Further `ffmpeg` calls wait for a free slot. `ffprobe`, `mediainfo` and `vainfo` are never limited, so library scans are not held up. |
| `FFMPEG_QUEUE_TIMEOUT` | `0` | Seconds an `ffmpeg` call may wait for a slot before it is rejected (`0` = wait indefinitely). Clients retry rejected calls on a new connection, so with several workers behind a load balancer the retry can reach a less busy worker. |
| `USE_SSL` | `false` | Serve gRPC over TLS. |
| `SSL_CERT_PATH` | `server.crt` | TLS certificate (chain). |
| `SSL_KEY_PATH` | `server.key` | TLS private key. |
| `GRPC_PORT` | `50051` | gRPC listen port. |
| `HTTP_PORT` | `8080` | Port for `/health` and `/metrics`. |
| `SHUTDOWN_GRACE_PERIOD` | `5` | Seconds running commands get to finish on shutdown before they are terminated. |
| `HEALTHCHECK_INTERVAL` | `60` | Seconds between health checks. |
| `HEALTHCHECK_TIMEOUT` | `60` | Seconds after which a hanging health check counts as failed. |
| `LOG_LEVEL` | `INFO` | Log level of the worker's own log. `DEBUG` also logs command output. This does not affect what is relayed to clients. |

### Client

The client reads its settings from a `grpc-ffmpeg.conf` file with `KEY=VALUE` lines
(`#` starts a comment, values may be quoted). It looks for the file next to the name it was
invoked as (e.g. next to the `ffmpeg` symlink), then next to the binary itself; set
`GRPC_FFMPEG_CONFIG` to use a different file. Environment variables with the same names
override the file.

```ini
# /opt/grpc-ffmpeg/grpc-ffmpeg.conf
GRPC_HOST=ffmpeg-workers
AUTH_TOKEN=change-me
FALLBACK_DIR=/usr/lib/jellyfin-ffmpeg
```

| Setting | Default | Description |
| --- | --- | --- |
| `GRPC_HOST` | `ffmpeg-workers` | Worker host name or IP address. |
| `GRPC_PORT` | `50051` | Worker gRPC port. |
| `AUTH_TOKEN` | `my_secret_token1` | Token sent to the worker; must match its `VALID_TOKEN`. |
| `USE_SSL` | `false` | Connect over TLS. |
| `CERTIFICATE_PATH` | `server.crt` | CA certificate used to verify the worker when `USE_SSL=true`. |
| `FALLBACK_DIR` | *(unset)* | Directory with local binaries of the same names (e.g. `/usr/lib/jellyfin-ffmpeg`). If no worker is reachable, the command runs there instead, so Jellyfin keeps working (and keeps starting) while the workers are down. |
| `RETRIES` | `5` | Attempts, with exponential backoff, while no worker is reachable or all are busy. Lower it when using `FALLBACK_DIR` so the fallback kicks in quickly. |
| `CONNECT_TIMEOUT` | `10` | Seconds to wait for a connection per attempt (Rust client only). |
| `LOG_FILE` | *(unset)* | Activity log: one line per command with its exit code and duration, the client's own messages (retries, auth errors, fallback), and the last lines of ffmpeg's stderr for failed commands. Useful because callers like Jellyfin often discard ffmpeg's stderr. A regular file is appended to and rotated at 1 MB. A named pipe (FIFO) is written without blocking, so nothing touches the disk and lines are dropped while nobody reads it. Rust client only. |

## Behaviour

- **Cancellation:** if a client stops or is killed (e.g. Jellyfin ends a transcode), the worker
  terminates the corresponding process. It sends SIGTERM first, then SIGKILL after 3 seconds.
  Dead clients are detected through HTTP/2 keepalive.
- **Retries:** clients retry (`RETRIES`, default 5) with exponential backoff while the worker
  is unreachable or busy, but never once a command has started. If no worker was reachable and
  `FALLBACK_DIR` is set, the command runs locally instead.
- **Shutdown:** on SIGTERM the worker stops accepting new calls and gives running commands
  `SHUTDOWN_GRACE_PERIOD` seconds before terminating them.
- **Compatibility:** clients and workers of different versions work together, so they can be
  upgraded in any order. Features that need both sides, such as stdin forwarding, turn on once
  both are updated.

## Monitoring

The worker serves the following on `HTTP_PORT`:

- `/health` returns `200` while the periodic self-test passes and `500` otherwise. The
  self-test runs `mediainfo` and converts a small sample video with `ffmpeg`. It is used as the
  Docker `HEALTHCHECK` and the Kubernetes readiness probe.
- `/metrics` serves Prometheus metrics:

| Metric | Description |
| --- | --- |
| `ffmpeg_process_count` | Running `ffmpeg` processes |
| `ffmpeg_queued_count` | `ffmpeg` calls waiting for a free slot |
| `ffmpeg_max_workers` | Configured `MAX_FFMPEG_WORKERS` |
| `ffmpeg_rejected_commands_total` | Calls rejected after `FFMPEG_QUEUE_TIMEOUT` |
| `ffmpeg_commands_total`, `ffprobe_commands_total`, `mediainfo_commands_total`, `vainfo_commands_total` | Commands executed per binary |

## Security

ffmpeg can read and write any file the worker can access, so anyone who can reach the gRPC
port can use the worker to do the same. Always set `VALID_TOKEN`, run the worker as an
unprivileged user (as the Kubernetes example does), and keep the port on a trusted network
or enable TLS with `USE_SSL`.

## Building from source

See [doc/BUILDING.md](doc/BUILDING.md) and [doc/RUNNING.md](doc/RUNNING.md). In short:

```bash
# Worker image
docker build -f docker/Dockerfile.server -t ffmpeg-worker .

# Rust client (needs protoc); static release binaries are built with
#   cargo build --release --target x86_64-unknown-linux-musl   (or aarch64-unknown-linux-musl)
# which needs clang and llvm, see src/client/rust/.cargo/config.toml
cd src/client/rust && cargo build --release
```

A Python client with the same behaviour is available in `src/client/grpc-ffmpeg.py`. It needs
Python 3.10+ and the packages in `requirements.txt`.

## Repository layout

```
grpc-ffmpeg/
├── src/
│   ├── proto/ffmpeg.proto       # gRPC API
│   ├── server/                  # worker (Python) + health check sample
│   └── client/
│       ├── rust/                # static client binary (released)
│       └── grpc-ffmpeg.py       # Python client
├── docker/                      # worker and client images
├── example_deployment/          # Kubernetes and docker compose examples
├── doc/                         # build and run instructions
├── .woodpecker/                 # CI pipelines
├── requirements.txt             # Python runtime dependencies
└── requirements-build.txt       # Python stub generation (grpcio-tools)
```

## License

This project is licensed under the GPL3 License - see the [LICENSE](LICENSE) file for details.

## Acknowledgements

- [rFFmpeg](https://github.com/joshuaboniface/rffmpeg)
- [FFmpeg](https://ffmpeg.org/) and [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg)
- [gRPC](https://grpc.io/)
- [Protobuf](https://developers.google.com/protocol-buffers)
