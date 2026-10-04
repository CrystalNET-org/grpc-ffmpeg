# grpc-ffmpeg

[![GPL3](https://img.shields.io/badge/license-GPLv3-blue)](#) [![last release](https://img.shields.io/github/v/release/CrystalNET-org/grpc-ffmpeg)](https://github.com/CrystalNET-org/grpc-ffmpeg/releases) [![Pipeline Status](https://ci.cluster.lan.crystalnet.org/api/badges/10/status.svg)](https://ci.cluster.lan.crystalnet.org/repos/10) [![Discord](https://dcbadge.limes.pink/api/server/Yj5AYwcGXu?style=flat)](https://discord.gg/Yj5AYwcGXu)

Run `ffmpeg` on other machines as if it were local. grpc-ffmpeg is a drop-in `ffmpeg` and
`ffprobe` that sends every call over gRPC to a worker, for example a node with a GPU. It is
built for Jellyfin, but works for any program that calls ffmpeg.

> **Using Jellyfin?** The [gRPC-ffmpeg plugin](https://github.com/CrystalNET-org/Jellyfin.Plugin.GrpcFfmpeg)
> sets up the client from Jellyfin's dashboard, with the official Jellyfin images and a live
> console of all ffmpeg calls. Run the workers as described below, then install the plugin.

## Why

Jellyfin runs ffmpeg on the machine Jellyfin runs on. In a cluster, that ties Jellyfin to
the node with the GPU, and that one node does all the transcoding.

With grpc-ffmpeg, Jellyfin stays where it is and needs no GPU. Its transcodes, probes and
image extractions run on workers on the GPU nodes. Workers sit behind a Service or load
balancer, so you can add more of them and spread the load.

Compared with [rFFmpeg](https://github.com/joshuaboniface/rffmpeg), which does the same over
SSH, grpc-ffmpeg uses gRPC with token authentication and optional TLS. Output is streamed live,
and cancelled commands are stopped on the worker.

## How it works

```
Jellyfin ──runs──▶ ffmpeg (client) ══gRPC══▶ worker ──runs──▶ jellyfin-ffmpeg
         ◀─ stdout/stderr/exit code ════════════════ ◀─
```

- **Worker:** a container with [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg)
  and the Intel and NVIDIA VAAPI drivers. It runs the commands it receives.
- **Client:** a small static binary installed as `ffmpeg` and `ffprobe` (and optionally
  `mediainfo` and `vainfo`). It runs the worker's binary of the same name.

The client behaves like a local ffmpeg:

- stdout and stderr are relayed byte for byte, in real time (including progress output).
- The exit code is passed through, including death by a signal.
- stdin is forwarded, so Jellyfin's `q` stops a remote transcode gracefully.
- Arguments reach the worker exactly as given, however they are quoted.
- If no worker is reachable, the command can run on a local ffmpeg instead (`FALLBACK_DIR`).

Files are not transferred: the worker reads and writes them directly.

## Requirements

- **Shared paths:** the worker reads and writes the files directly, so everything ffmpeg
  works on must be mounted at the same paths on Jellyfin and on every worker, e.g. over NFS or
  a shared volume:
  - the media library,
  - Jellyfin's cache directory, including the transcode directory,
  - Jellyfin's temp directory, `/tmp/jellyfin` by default, where image extraction and trickplay
    write their output.

  The Jellyfin plugin's connection test checks these directories.
- **One kind of GPU per pool:** Jellyfin builds its hardware-accelerated commands for one
  acceleration type (QSV, VAAPI, NVENC, …) and checks the hardware once at startup, through
  whichever worker answers. All workers behind the same address need the same kind of GPU.
- **Matching ffmpeg version:** Jellyfin enables ffmpeg options based on the version it
  detects. Run workers with the same ffmpeg major version as Jellyfin's own ffmpeg:

  | Jellyfin | Worker image |
  | --- | --- |
  | 12.x | `8.1.3-7.8` or later (jellyfin-ffmpeg 8) |
  | 10.11 | `7.1.4-7.7` (jellyfin-ffmpeg 7, last release for 10.11) |

- **Platforms:** the worker image is amd64. The client is available for Linux amd64 and
  arm64, and Windows amd64.

## Getting started

### 1. Run a worker

Worker images are published as `ghcr.io/crystalnet-org/ffmpeg-worker`, tagged with each
release (e.g. `8.1.3-7.8`) and `dev` for the latest build of `main`.

```bash
docker run -d --name ffmpeg-worker \
  -p 50051:50051 -p 8080:8080 \
  -e VALID_TOKEN=change-me \
  --user 1000:1000 --group-add "$(getent group render | cut -d: -f3)" \
  --device /dev/dri:/dev/dri \
  -v /srv/media:/media \
  -v /srv/jellyfin/cache:/cache \
  -v /srv/jellyfin/tmp:/tmp/jellyfin \
  ghcr.io/crystalnet-org/ffmpeg-worker:8.1.3-7.8   # or the latest release tag
```

> **Always set `VALID_TOKEN`.** Without it, anyone who can reach port 50051 can run ffmpeg
> with the worker's access to your files (see [Security](#security)).

Mount the same paths that Jellyfin uses, including its temp directory, and run the worker as
Jellyfin's user (`--user`) so both can work with each other's files. `--group-add` with the
host's `render` group ID gives that user access to the GPU (the group that owns
`/dev/dri/renderD*`).
For NVIDIA GPUs, run the container with the NVIDIA container runtime instead of passing
`/dev/dri`; with several NVIDIA GPUs in one worker, set `CUDA_DEVICES=auto` so transcodes
use all of them, not just the first. The image selects Intel's `iHD` VAAPI driver; for AMD GPUs set
`LIBVA_DRIVER_NAME=radeonsi`, for older Intel GPUs `LIBVA_DRIVER_NAME=i965`.

For Kubernetes, [`example_deployment/kubernetes`](example_deployment/kubernetes) has a
DaemonSet with one worker per GPU node and a Service named `ffmpeg-workers`. Adjust the
GPU resource, node selection and volumes to your cluster.

Check that the worker is healthy: `curl http://<worker>:8080/health` returns `200`.

### 2. Connect Jellyfin

**With the Jellyfin plugin (recommended).** The
[gRPC-ffmpeg plugin](https://github.com/CrystalNET-org/Jellyfin.Plugin.GrpcFfmpeg) brings the
client along and points Jellyfin at it. It works with the official Jellyfin images and
packages: install it from the plugin catalog and enter the worker's address and token on its
settings page. It also shows a live console of all ffmpeg calls.

**By hand.** Download the client for your platform (`grpc-ffmpeg-client-amd64`,
`grpc-ffmpeg-client-arm64` or `grpc-ffmpeg-client-windows-amd64.exe`) from the
[latest release](https://github.com/CrystalNET-org/grpc-ffmpeg/releases/latest), and install it
as `ffmpeg` and `ffprobe` in the same directory:

```bash
mkdir -p /opt/grpc-ffmpeg
curl -L -o /opt/grpc-ffmpeg/grpc-ffmpeg-client \
  https://github.com/CrystalNET-org/grpc-ffmpeg/releases/latest/download/grpc-ffmpeg-client-amd64
chmod +x /opt/grpc-ffmpeg/grpc-ffmpeg-client
ln -s grpc-ffmpeg-client /opt/grpc-ffmpeg/ffmpeg
ln -s grpc-ffmpeg-client /opt/grpc-ffmpeg/ffprobe
```

On Windows, copy the `.exe` to `ffmpeg.exe` and `ffprobe.exe` in the same directory instead.

Configure it with a `grpc-ffmpeg.conf` in the same directory:

```ini
GRPC_HOST=ffmpeg-workers        # worker host name, Service or load balancer
AUTH_TOKEN=change-me            # the worker's VALID_TOKEN
FALLBACK_DIR=/usr/lib/jellyfin-ffmpeg
RETRIES=2
```

Then set Jellyfin's ffmpeg path to `/opt/grpc-ffmpeg/ffmpeg`, with the `JELLYFIN_FFMPEG`
environment variable or under *Dashboard → Playback → Transcoding*. Jellyfin refuses to start
if its ffmpeg check fails, so keep `FALLBACK_DIR` pointing at a local ffmpeg.

### 3. Check it

```bash
/opt/grpc-ffmpeg/ffmpeg -version
```

This prints the worker's ffmpeg version. After a restart, Jellyfin's log shows the same
version in `Found ffmpeg version …`, and the worker's log lists every command it runs.

## Configuration

### Worker

| Variable | Default | Description |
| --- | --- | --- |
| `VALID_TOKEN` | *(unset)* | Token clients must send. When set, calls without it are rejected with `UNAUTHENTICATED`. **When unset, anyone who can reach the gRPC port can run commands**; a warning is logged. |
| `BINARY_PATH_PREFIX` | `/usr/lib/jellyfin-ffmpeg/` | Directory containing the binaries. Only `ffmpeg`, `ffprobe`, `mediainfo` and `vainfo` can be run; those not in this directory (such as `mediainfo`, which is not part of jellyfin-ffmpeg) are looked up in `PATH`. |
| `MAX_FFMPEG_WORKERS` | `10` | Maximum number of concurrent `ffmpeg` processes with an input (`-i`: transcodes, image extraction; `0` = unlimited). Further ones wait for a free slot. Queries such as `ffmpeg -version` or `-encoders`, and `ffprobe`, `mediainfo` and `vainfo`, are never limited, so Jellyfin's startup checks and library scans are not held up. |
| `CUDA_DEVICES` | *(unset)* | NVIDIA GPUs to spread transcodes over, as indexes or UUIDs (`0,1`), or `auto` for all GPUs `nvidia-smi` lists. Jellyfin always uses CUDA device 0 (`-init_hw_device cuda=cu:0`), so on a worker with several GPUs every transcode would run on the first one. With this set, each command that uses CUDA gets the GPU running the fewest such commands, through `CUDA_VISIBLE_DEVICES`; other commands are unchanged. Raise `MAX_FFMPEG_WORKERS` to match the number of GPUs. |
| `FFMPEG_QUEUE_TIMEOUT` | `0` | Seconds an `ffmpeg` call may wait for a slot before it is rejected (`0` = wait indefinitely). Clients retry rejected calls on a new connection, so behind a load balancer the retry can reach a less busy worker. |
| `USE_SSL` | `false` | Serve gRPC over TLS. |
| `SSL_CERT_PATH` | `server.crt` | TLS certificate (chain). |
| `SSL_KEY_PATH` | `server.key` | TLS private key. |
| `GRPC_PORT` | `50051` | gRPC listen port, on IPv6 and IPv4 where the host has IPv6, otherwise on IPv4 only. |
| `HTTP_PORT` | `8080` | Port for `/health` and `/metrics`. |
| `SHUTDOWN_GRACE_PERIOD` | `5` | Seconds running commands get to finish on shutdown before they are terminated. |
| `HEALTHCHECK_INTERVAL` | `60` | Seconds between health checks. |
| `HEALTHCHECK_TIMEOUT` | `60` | Seconds after which a hanging health check counts as failed. |
| `LOG_LEVEL` | `INFO` | Log level of the worker's own log. `DEBUG` also logs command output. This does not affect what is relayed to clients. |

### Client

The client reads `grpc-ffmpeg.conf`, a file with `KEY=VALUE` lines (`#` starts a comment,
values may be quoted). It looks for the file next to the name it was invoked as (e.g. next to
the `ffmpeg` symlink), then next to the binary. `GRPC_FFMPEG_CONFIG` points it to another
file. Environment variables with the same names override the file.

| Setting | Default | Description |
| --- | --- | --- |
| `GRPC_HOST` | `ffmpeg-workers` | Worker host name or IP address (IPv6 with or without brackets). |
| `GRPC_PORT` | `50051` | Worker gRPC port. |
| `AUTH_TOKEN` | `my_secret_token1` | Token sent to the worker; must match its `VALID_TOKEN`. |
| `USE_SSL` | `false` | Connect over TLS. |
| `CERTIFICATE_PATH` | `server.crt` | CA certificate used to verify the worker when `USE_SSL=true`. |
| `FALLBACK_DIR` | *(unset)* | Directory with local binaries of the same names (e.g. `/usr/lib/jellyfin-ffmpeg`). If no worker is reachable or the worker rejects the token, the command runs there instead, so Jellyfin keeps working, and starting, while the workers are down or misconfigured. |
| `RETRIES` | `5` | Attempts while no worker is reachable or all are busy, waiting 1, 2, 4 and then 5 seconds between them. Lower it when using `FALLBACK_DIR` so the fallback kicks in quickly. |
| `CONNECT_TIMEOUT` | `10` | Seconds to wait for a connection per attempt. |
| `LOG_FILE` | *(unset)* | Activity log: one line per command with its exit code and duration, the client's own messages (retries, authentication errors, fallback), and the last lines of ffmpeg's stderr for failed commands. Useful because callers like Jellyfin often discard ffmpeg's stderr. A regular file is appended to and rotated at 1 MB. A named pipe (FIFO) is written without blocking, so nothing touches the disk and lines are dropped while nobody reads it. |
| `CLASS_ADDRESSES` | *(unset)* | **Experimental.** Worker pools per hardware class, e.g. `nvidia=workers-nvidia:50051;intel=workers-intel:50051`. Each command is classified from its own arguments: `-init_hw_device cuda=…` is `nvidia`; `qsv=…`, or `vaapi=…` with `driver=iHD`/`i965`, is `intel`. It then goes to that class's address. Commands without hardware arguments (probes, software transcodes, AMD VAAPI), and classes without an entry, go to `GRPC_HOST`. Retries, the fallback and the activity log work per address; log lines show the class as `run [nvidia host:port]: …`. Set by the Jellyfin plugin's hardware classes. |

## Behaviour

- **Cancellation:** if a client stops or is killed (e.g. Jellyfin ends a transcode), the
  worker terminates the process: SIGTERM first, SIGKILL after 3 seconds. Dead clients are
  detected through HTTP/2 keepalive.
- **Retries:** clients retry with exponential backoff while the worker is unreachable or busy,
  but never once a command has started. Workers tell the client as soon as the process runs,
  so a command that fails afterwards, e.g. because its worker died, is never run a second
  time elsewhere or on the fallback, even if it had printed nothing yet.
- **Fallback:** with `FALLBACK_DIR` set, a command runs locally if no worker was reachable or
  the token was rejected. After a run found no worker, further runs within 20 seconds go to
  the fallback right away, so a burst of calls (like Jellyfin's startup checks) does not wait
  through the retries each time. The activity log records every fallback with its reason.
- **Shutdown:** on SIGTERM the worker stops accepting new calls and gives running commands
  `SHUTDOWN_GRACE_PERIOD` seconds before terminating them.
- **Versions:** clients and workers of different versions work together, so they can be
  upgraded in any order. Features that need both sides, such as stdin forwarding, turn on once
  both support them.

## Monitoring

The worker serves the following on `HTTP_PORT`:

- `/health` returns `200` while the periodic self-test passes and `500` otherwise. The
  self-test runs `mediainfo` and converts a small sample video with `ffmpeg`. It serves as the
  Docker `HEALTHCHECK` and the Kubernetes readiness probe.
- `/metrics` serves Prometheus metrics:

| Metric | Description |
| --- | --- |
| `worker_healthy` | `1` while the self-test passes (as `/health`), `0` otherwise |
| `ffmpeg_process_count` | Running `ffmpeg` processes |
| `ffmpeg_queued_count` | `ffmpeg` calls waiting for a free slot |
| `ffmpeg_max_workers` | Configured `MAX_FFMPEG_WORKERS` |
| `ffmpeg_gpu_process_count{device}` | Running CUDA commands per GPU (with `CUDA_DEVICES`) |
| `ffmpeg_rejected_commands_total` | Calls rejected after `FFMPEG_QUEUE_TIMEOUT` |
| `ffmpeg_commands_total`, `ffprobe_commands_total`, `mediainfo_commands_total`, `vainfo_commands_total` | Commands run, per binary |

[`example_deployment/grafana/grpc-ffmpeg-workers.json`](example_deployment/grafana/grpc-ffmpeg-workers.json)
is a Grafana dashboard for these metrics: health, load against `MAX_FFMPEG_WORKERS`, queueing
and rejections, calls per binary and worker, restarts, and running commands per GPU. Import it
and pick the Prometheus data source. It filters by the scrape `job` (e.g. one per worker pool)
and a `node` label; if your scrape config adds no `node` label, replace it with `instance` or
`pod` in the dashboard.

## Security

ffmpeg can read and write any file the worker can access, so anyone who can reach the gRPC
port can use the worker to do the same. Always set `VALID_TOKEN`, run the worker as an
unprivileged user (as the Kubernetes example does), and keep the port on a trusted network or
enable TLS with `USE_SSL`.

## Releases

Release tags are `<jellyfin-ffmpeg version>-<release number>`: `8.1.3-7.8` bundles
jellyfin-ffmpeg 8.1.3-1. Each tag publishes the worker image and the client binaries. New
jellyfin-ffmpeg versions, and changes to the worker, the client or the protocol, are released
automatically.

## Contributing

Bug reports, ideas and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for
the development setup, how to build, and how releases are made.

## License

GPL-3.0, see [LICENSE](LICENSE).

## Acknowledgements

- [rFFmpeg](https://github.com/joshuaboniface/rffmpeg), the inspiration for this project
- [FFmpeg](https://ffmpeg.org/) and [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg)
- [gRPC](https://grpc.io/) and [Protocol Buffers](https://protobuf.dev/)
