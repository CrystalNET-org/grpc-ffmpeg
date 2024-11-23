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

The following environment variables can be set to configure the server and client:

- `VALID_TOKEN`: The authentication token for the gRPC server (default: `my_secret_token`).
- `ALLOWED_BINARIES`: The binaries allowed to be executed (default: `['ffmpeg', 'ffprobe', 'vainfo']`).
- `BINARY_PATH_PREFIX`: The path prefix for the binaries (default: `/usr/bin/`).
- `SSL_KEY_PATH`: The path to the SSL key file (default: `server.key`).
- `SSL_CERT_PATH`: The path to the SSL certificate file (default: `server.crt`).
- `USE_SSL`: Whether to use SSL (default: `false`).
- `GRPC_HOST`: The hostname for the gRPC client to connect to (default: `ffmpeg-workers`).
- `GRPC_PORT`: The port for the gRPC client to connect to (default: `50051`).
- `CERTIFICATE_PATH`: The path to the SSL certificate for the client (default: `server.crt`).
- `AUTH_TOKEN`: The authentication token for the client (default: `my_secret_token1`).

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
python src/client/grpc-ffmpeg.py "ffmpeg -i input.mp4 output.mp4"
```

### License

This project is licensed under the GPL3 License - see the [LICENSE](LICENSE) file for details.

### Acknowledgements

- [rFFmpeg](https://github.com/joshuaboniface/rffmpeg)
- [FFmpeg](https://ffmpeg.org/)
- [gRPC](https://grpc.io/)
- [Protobuf](https://developers.google.com/protocol-buffers)
