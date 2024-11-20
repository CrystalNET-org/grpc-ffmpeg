# How to Build the Project

This guide explains how to build and set up the `grpc-ffmpeg` project on your local machine.

---

## Prerequisites

Before building the project, ensure you have the following installed:

1. **Python** (3.7 or later)
2. **FFmpeg** (latest version)
3. **Docker** (optional, for containerized setup)
4. **Protocol Buffers Compiler** (`protoc`)

---

## Steps to Build the Project

### 1. Clone the Repository

First, clone the `grpc-ffmpeg` repository to your local machine:

```bash
git clone https://github.com/CrystalNET-org/grpc-ffmpeg.git
cd grpc-ffmpeg
```

### 2. Create a Virtual Environment

Set up a Python virtual environment to manage dependencies:

```bash
python3 -m venv venv
source venv/bin/activate  # On Linux/MacOS
# On Windows:
# venv\Scripts\activate
```

### 3. Install Python Dependencies

Install the required Python packages using `pip`:

```bash
pip install -r requirements.txt
```

### 4. Compile Protocol Buffers

Ensure the protoc compiler is installed. Then, generate the gRPC Python code:

```bash
python -m grpc_tools.protoc -I=src/proto --python_out=src/proto --grpc_python_out=src/proto src/proto/ffmpeg.proto
```

### 5. Configure Environment Variables

Set up the required environment variables. You can either:

Set them manually, or
Create a .env file in the project root.
Example .env file:

```env
FFMPEG_PATH=/usr/bin/ffmpeg
SSL_CERT_PATH=/path/to/ssl/cert.pem
AUTH_TOKEN=your_auth_token_here
```

### 6. Build and Run the Project
#### 6.1 Running Locally
To run the server locally:

```bash
python src/server/server.py
```

To run the client:

```bash
python src/client/client.py
```

#### 6.2 Running with Docker
To build and run the project using Docker, ensure Docker is installed and execute:

```bash
docker build -t grpc-ffmpeg-server -f docker/Dockerfile.server .
docker build -t grpc-ffmpeg-client -f docker/Dockerfile.client .

docker run -d --name grpc-ffmpeg-server -p 50051:50051 grpc-ffmpeg-server
docker run -it --rm grpc-ffmpeg-client
```

Alternatively, use Docker Compose:

```bash
docker-compose -f example_deployment/docker_compose/docker-compose.yml up --build
```

## Troubleshooting
### Missing Dependencies

If you encounter missing dependencies, update pip and reinstall:

```bash
pip install --upgrade pip
pip install -r requirements.txt
```

### Protocol Buffers Compilation Errors

Ensure the protoc compiler is installed and accessible in your PATH:

```bash
protoc --version
```

### Docker Issues
Verify Docker is installed and running:

```bash
docker info
```

You’re now ready to build and use the grpc-ffmpeg project. For more detailed usage examples, refer to the Usage Guide.
