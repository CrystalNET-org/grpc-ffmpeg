ARG PROTOC_VERSION=28.2
FROM debian:bookworm-slim AS builder
ARG PROTOC_VERSION

# set our working directory within the build context
WORKDIR /app

# install needed build-time dependencies
RUN apt-get update && apt-get install --no-install-recommends -y \
    curl \
    unzip \
    python3 \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

RUN curl -LO https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip && \
    unzip protoc-${PROTOC_VERSION}-linux-x86_64.zip -d /app/.local

# Upgrade pip (we throw away the build context anyways, so we are fine with breaking our system ^^)
RUN pip3 install --break-system-packages --upgrade pip

# move our protobuf definition file into our context
COPY proto/ffmpeg.proto .

# compile the .proto-file for later use from within python
RUN python3 -m pip install --break-system-packages grpcio grpcio-tools 
RUN PATH="$PATH:$HOME/app/.local/bin" python3 -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. ffmpeg.proto

# Final stage
FROM debian:bookworm-slim

# Enable non-free repositories
RUN sed -i 's/Components: main/Components: main contrib non-free/' /etc/apt/sources.list.d/debian.sources

# Define the ARG for the jellyfin-ffmpeg version
# renovate: datasource=github-releases depName=jellyfin/jellyfin-ffmpeg versioning=loose
ARG JELLYFIN_FFMPEG_VERSION=7.0.2-5

# Install packages that are needed for the runtime environment
RUN apt-get update && apt-get install --no-install-recommends -y \
    libssl3 \
    libc-bin \
    ca-certificates \
    wget \
    python3-pip \
    intel-media-va-driver-non-free \
    i965-va-driver \
    intel-opencl-icd \
    nvidia-vaapi-driver \
    libbluray2 \
    libllvm16 \
    mediainfo \
    libmp3lame0 \
    libopenmpt0 \
    libvpx7 \
    libwebp7 \
    libwebpmux3 \
    libx264-164 \
    libx265-199 \
    libzvbi0

# setup python specific environment
COPY requirements.txt /app/
RUN pip3 install --break-system-packages --upgrade pip
RUN python3 -m pip install --break-system-packages -r /app/requirements.txt

# Download and install jellifin's fork of ffmpeg which comes with additional codecs and improved hw accelleration routines
RUN wget https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/jellyfin-ffmpeg7_${JELLYFIN_FFMPEG_VERSION}-bookworm_amd64.deb && \
    dpkg -i jellyfin-ffmpeg6_${JELLYFIN_FFMPEG_VERSION}-bookworm_amd64.deb && \
    apt-get install -f && \
    rm jellyfin-ffmpeg6_${JELLYFIN_FFMPEG_VERSION}-bookworm_amd64.deb

# Copy the build artifact from the build stage
COPY /server/ /app/
COPY /client/ /client/

COPY --from=builder /app/ffmpeg* /app/
COPY --from=builder /app/ffmpeg* /client/

RUN chmod a+x /app/grpc-ffmpeg.py
RUN chmod a+x /client/grpc-ffmpeg.py

# Expose the port the service runs on
EXPOSE 50051 8080

# Set environment variables for Intel and Nvidia hardware acceleration
ENV LIBVA_DRIVER_NAME=iHD
ENV LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=compute,video,utility

# Run the binary
CMD ["/app/grpc-ffmpeg.py"]
