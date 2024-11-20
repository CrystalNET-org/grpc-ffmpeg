### Running the Server

#### Using Python directly:

```bash
python src/server/grpc-ffmpeg.py
```

#### Using Docker:

Build the Docker image:

```bash
docker build -t grpc-ffmpeg-server -f docker/Dockerfile.server .
```

Run the Docker container:

```bash
docker run -p 50051:50051 -p 8080:8080 grpc-ffmpeg-server
```

### Running the Client

```bash
python src/client/grpc-ffmpeg.py <ffmpeg_command>
```