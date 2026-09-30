#!/usr/bin/env python3
import grpc
import ffmpeg_pb2
import ffmpeg_pb2_grpc
import asyncio
import os
import shlex
import sys

# Configuration
CERTIFICATE_PATH = os.getenv("CERTIFICATE_PATH", "server.crt")
AUTH_TOKEN = os.getenv("AUTH_TOKEN", "my_secret_token1")
GRPC_HOST = os.getenv("GRPC_HOST", "ffmpeg-workers")
GRPC_PORT = os.getenv("GRPC_PORT", "50051")
USE_SSL = os.getenv("USE_SSL", "false").lower() == "true"

# Detect a dead server on long-running, quiet streams. The interval matches
# the minimum gRPC servers accept by default, so older servers (which do not
# relax that limit) will not reject the pings.
CHANNEL_OPTIONS = [
    ("grpc.keepalive_time_ms", 300_000),
    ("grpc.keepalive_timeout_ms", 20_000),
    ("grpc.http2.max_pings_without_data", 0),
]


def create_channel(use_ssl):
    target = f"{GRPC_HOST}:{GRPC_PORT}"
    if use_ssl:
        with open(CERTIFICATE_PATH, "rb") as f:
            trusted_certs = f.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=trusted_certs)
        return grpc.aio.secure_channel(target, credentials, options=CHANNEL_OPTIONS)
    return grpc.aio.insecure_channel(target, options=CHANNEL_OPTIONS)


async def run_command(args, use_ssl):
    # `command` is for servers that predate the `args` field
    request = ffmpeg_pb2.CommandRequest(command=shlex.join(args), args=args)
    # Sent as plain metadata so the token also works without SSL
    metadata = (("authorization", AUTH_TOKEN),)

    exit_code = 1  # Stays 1 if the server never reports an exit code
    max_retries = 5
    base_delay = 1.0

    async with create_channel(use_ssl) as channel:
        stub = ffmpeg_pb2_grpc.FFmpegServiceStub(channel)
        for attempt in range(max_retries):
            received_response = False
            try:
                async for response in stub.ExecuteCommand(request, metadata=metadata):
                    received_response = True
                    if response.binary_output:
                        sys.stdout.buffer.write(response.binary_output)
                        sys.stdout.buffer.flush()
                    elif response.output:
                        if response.stream == "stdout":
                            sys.stdout.write(response.output)
                            sys.stdout.flush()
                        elif response.stream == "stderr":
                            sys.stderr.write(response.output)
                            sys.stderr.flush()
                    elif response.stream == "exit_code":
                        exit_code = response.exit_code
                return exit_code

            except grpc.aio.AioRpcError as e:
                # Only retry if the command never started; re-running a
                # partially streamed command would duplicate its output.
                if (
                    e.code() == grpc.StatusCode.UNAVAILABLE
                    and not received_response
                    and attempt < max_retries - 1
                ):
                    delay = base_delay * (2 ** attempt)
                    sys.stderr.write(f"Server unavailable, retrying in {delay:.1f} seconds... (Attempt {attempt + 1}/{max_retries})\n")
                    await asyncio.sleep(delay)
                    continue
                sys.stderr.write(f"gRPC error after {attempt + 1} attempts: {e.code().name}: {e.details()}\n")
                return 1
            except Exception as e:
                sys.stderr.write(f"An unexpected error occurred: {e}\n")
                return 1

    sys.stderr.write("Command failed after reaching max retries.\n")
    return 1


if __name__ == "__main__":
    # Busybox style: the name this script is invoked as (e.g. a symlink named
    # "ffmpeg" or "ffprobe") is the remote binary to run.
    script_name = os.path.basename(sys.argv[0])
    exit_code = asyncio.run(run_command([script_name] + sys.argv[1:], USE_SSL))
    sys.exit(exit_code)
