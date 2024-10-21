# GRPC FFmpeg Service

## Overview

This project provides a gRPC-based service for executing FFmpeg commands. It consists of a server that processes FFmpeg commands and a client that sends commands to the server. The server is designed to be secure and configurable, with options for SSL and token-based authentication.

## Directory Structure

```
.
├── proto
│   └── ffmpeg.proto        # Protobuf definition file
├── server
│   └── grpc-ffmpeg.py      # Server script
├── client
│   └── grpc-ffmpeg.py      # Client script
│   └── Dockerfile          # Dockerfile for building and running the client
├── Dockerfile              # Dockerfile for building and running the server
├── README.md               # Project documentation
└── requirements.txt        # Python dependencies
```

## Getting Started

### Prerequisites

- Python 3.6+
- Docker

### Building the Project

#### 1. Clone the repository:

```bash
git clone https://github.com/yourusername/grpc-ffmpeg-service.git
cd grpc-ffmpeg-service
```

#### 2. Install Python dependencies:

```bash
pip install -r requirements.txt
```

#### 3. Compile the Protobuf definition:

```bash
python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. proto/ffmpeg.proto
```

### Running the Server

#### Using Python directly:

```bash
python server/grpc-ffmpeg.py
```

#### Using Docker:

Build the Docker image:

```bash
docker build -t grpc-ffmpeg-service .
```

Run the Docker container:

```bash
docker run -p 50051:50051 -p 8080:8080 grpc-ffmpeg-service
```

### Running the Client

```bash
python client/grpc-ffmpeg.py <ffmpeg_command>
```

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
python server/grpc-ffmpeg.py
```

#### Client

Send a command to the server with SSL:

```bash
export USE_SSL=true
export CERTIFICATE_PATH=/path/to/server.crt
export AUTH_TOKEN=my_secret_token1
python client/grpc-ffmpeg.py "ffmpeg -i input.mp4 output.mp4"
```

### License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

### Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) for details on our code of conduct, and the process for submitting pull requests.

### Acknowledgements

- [FFmpeg](https://ffmpeg.org/)
- [gRPC](https://grpc.io/)
- [Protobuf](https://developers.google.com/protocol-buffers)
