FROM debian:bookworm-slim AS builder

# Setzen der Arbeitsverzeichnis im Container
WORKDIR /app

# Installieren von notwendigen Paketen
RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Upgrade pip
RUN pip3 install --break-system-packages --upgrade pip

# Kopieren der Protobuf-Definition in das Arbeitsverzeichnis im Container
COPY ffmpeg.proto .

# Kompilieren der .proto-Datei für Python
RUN python3 -m pip install --break-system-packages grpcio grpcio-tools 
RUN python3 -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. ffmpeg.proto

# Final stage
FROM debian:bookworm-slim

# Enable non-free repositories
RUN sed -i 's/Components: main/Components: main contrib non-free/' /etc/apt/sources.list.d/debian.sources

RUN apt-get update && apt-get install -y \
    libssl3 \
    libc-bin \
    ca-certificates \
    wget \
    python3-pip

RUN pip3 install --break-system-packages --upgrade pip
RUN python3 -m pip install --break-system-packages grpcio grpcio-tools aiofiles

# Copy the build artifact from the build stage
COPY grpc-ffmpeg.py /app/

COPY --from=builder /app/ffmpeg* /app/

RUN chmod a+x /app/grpc-ffmpeg.py

# Run the binary
CMD ["/app/grpc-ffmpeg.py"]
