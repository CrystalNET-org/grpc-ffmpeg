# GRPC FFmpeg Service

[![GPL3](https://img.shields.io/badge/license-GPLv3-blue)](#) [![last release](https://img.shields.io/github/v/release/CrystalNET-org/grpc-ffmpeg)](https://github.com/CrystalNET-org/grpc-ffmpeg/releases) [![Pipeline Status](https://ci.cluster.lan.crystalnet.org/api/badges/10/status.svg)](https://ci.cluster.lan.crystalnet.org/repos/10) [![Discord](https://dcbadge.limes.pink/api/server/Yj5AYwcGXu?style=flat)](https://discord.gg/Yj5AYwcGXu)
## Overview

This project provides a gRPC-based service for executing FFmpeg commands. It consists of a server that processes FFmpeg commands and a client that sends commands to the server. The server is designed to be secure and configurable, with options for SSL and token-based authentication.

## Directory Structure

```
grpc-ffmpeg/
├── src/
│   ├── server/
│   ├── client/
│   └── proto/
├── docs/
├── docker/
├── scripts/
├── LICENSE
├── README.md
├── requirements.txt
```

## Getting Started

### Prerequisites

- Python 3.6+
- Docker

Shared tmp directories between the client and worker containers (for jellyfin this would be the cache directory)

### Environment Variables

#### Server

- `VALID_TOKEN`: Authentication token clients must send. When set, calls without a matching token are rejected with `UNAUTHENTICATED`; when unset, authentication is disabled (a warning is logged).
- `BINARY_PATH_PREFIX`: The path prefix for the binaries (default: `/usr/lib/jellyfin-ffmpeg/`). Only `ffmpeg`, `ffprobe`, `mediainfo` and `vainfo` may be executed.
- `USE_SSL`: Whether to use SSL (default: `false`).
- `SSL_KEY_PATH`: The path to the SSL key file (default: `server.key`).
- `SSL_CERT_PATH`: The path to the SSL certificate file (default: `server.crt`).
- `GRPC_PORT`: gRPC listen port (default: `50051`).
- `HTTP_PORT`: Port for `/health` and `/metrics` (default: `8080`).
- `MAX_FFMPEG_WORKERS`: Maximum number of concurrent `ffmpeg` processes (default: `10`, `0` = unlimited). Further `ffmpeg` calls wait for a free slot; `ffprobe`, `mediainfo` and `vainfo` are never limited, so library scans are not held up.
- `FFMPEG_QUEUE_TIMEOUT`: Seconds an `ffmpeg` call may wait for a free slot before it is rejected (default: `0` = wait indefinitely). Rejected calls are retried by the clients on a new connection, so with several workers behind a load balancer they can land on a less busy one.
- `SHUTDOWN_GRACE_PERIOD`: Seconds in-flight commands get to finish on shutdown before they are terminated (default: `5`).
- `HEALTHCHECK_INTERVAL` / `HEALTHCHECK_TIMEOUT`: Health check interval and timeout in seconds (default: `60` / `60`).
- `LOG_LEVEL`: Python log level (default: `INFO`; `DEBUG` logs ffmpeg's stderr).

The clients behave like a local ffmpeg: stdout and stderr are relayed byte for byte, the exit status (including death by a signal) is passed through, and stdin is forwarded, so Jellyfin's `q` stops a remote transcode gracefully. If a client disconnects (for example because it was killed), the server terminates the corresponding ffmpeg process.

Metrics of note: `ffmpeg_process_count`, `ffmpeg_queued_count`, `ffmpeg_max_workers` and `ffmpeg_rejected_commands_total`.

#### Client

- `GRPC_HOST`: The hostname for the gRPC client to connect to (default: `ffmpeg-workers`).
- `GRPC_PORT`: The port for the gRPC client to connect to (default: `50051`).
- `USE_SSL`: Whether to use SSL (default: `false`).
- `CERTIFICATE_PATH`: The path to the SSL certificate for the client (default: `server.crt`).
- `AUTH_TOKEN`: The authentication token for the client (default: `my_secret_token1`). Must match the server's `VALID_TOKEN`.

The client runs the binary it is invoked as, so install it (or symlink it) under the names `ffmpeg` and `ffprobe`.

### Example Usage

#### Server

Start the server with SSL:

```bash
export USE_SSL=true
export SSL_KEY_PATH=/path/to/server.key
export SSL_CERT_PATH=/path/to/server.crt
export VALID_TOKEN=my_secret_token1
python src/server/grpc-ffmpeg.py
```

#### Client

Send a command to the server with SSL:

```bash
export USE_SSL=true
export CERTIFICATE_PATH=/path/to/server.crt
export AUTH_TOKEN=my_secret_token1
ln -s "$PWD/src/client/grpc-ffmpeg.py" /usr/local/bin/ffmpeg
ffmpeg -i input.mp4 output.mp4
```

### License

This project is licensed under the GPL3 License - see the [LICENSE](LICENSE) file for details.

### Acknowledgements

- [rFFmpeg](https://github.com/joshuaboniface/rffmpeg)
- [FFmpeg](https://ffmpeg.org/)
- [gRPC](https://grpc.io/)
- [Protobuf](https://developers.google.com/protocol-buffers)
