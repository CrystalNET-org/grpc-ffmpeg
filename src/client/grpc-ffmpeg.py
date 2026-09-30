#!/usr/bin/env python3
import grpc
import ffmpeg_pb2
import ffmpeg_pb2_grpc
import asyncio
import os
import shlex
import signal
import sys
import threading

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


RETRYABLE_CODES = (grpc.StatusCode.UNAVAILABLE, grpc.StatusCode.RESOURCE_EXHAUSTED)
STDIN_CHUNK_SIZE = 64 * 1024


def start_stdin_reader():
    """Read stdin on a daemon thread (a blocking read cannot be cancelled) into
    a bounded queue shared by all attempts. b"" marks EOF."""
    loop = asyncio.get_running_loop()
    queue = asyncio.Queue(maxsize=16)

    def reader():
        while True:
            try:
                data = os.read(sys.stdin.fileno(), STDIN_CHUNK_SIZE)
            except (OSError, ValueError):
                data = b""
            try:
                asyncio.run_coroutine_threadsafe(queue.put(data), loop).result()
            except RuntimeError:
                return  # Event loop is gone
            if not data:
                return

    threading.Thread(target=reader, daemon=True).start()
    return queue


async def execute_requests(request, stdin_queue):
    """Request stream for Execute: the command, then stdin data. Returning
    half-closes the call, which closes the remote process's stdin."""
    yield ffmpeg_pb2.ExecuteRequest(request=request)
    while True:
        data = await stdin_queue.get()
        if not data:
            stdin_queue.put_nowait(b"")  # Keep EOF visible to later attempts
            return
        yield ffmpeg_pb2.ExecuteRequest(stdin=data)


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
    request = ffmpeg_pb2.CommandRequest(
        command=shlex.join(args), args=args, raw_stderr=True
    )
    # Sent as plain metadata so the token also works without SSL
    metadata = (("authorization", AUTH_TOKEN),)

    stdin_queue = start_stdin_reader()
    # Servers without the Execute RPC do not support stdin forwarding
    forward_stdin = True

    exit_code = 1  # Stays 1 if the server never reports an exit code
    max_retries = 5
    base_delay = 1.0
    attempt = 0

    while True:
        received_response = False
        # A new connection per attempt, so behind a load balancer a retry can
        # land on a different worker
        async with create_channel(use_ssl) as channel:
            stub = ffmpeg_pb2_grpc.FFmpegServiceStub(channel)
            try:
                if forward_stdin:
                    call = stub.Execute(
                        execute_requests(request, stdin_queue), metadata=metadata
                    )
                else:
                    call = stub.ExecuteCommand(request, metadata=metadata)
                async for response in call:
                    received_response = True
                    if response.binary_output:
                        out = sys.stderr if response.stream == "stderr" else sys.stdout
                        out.buffer.write(response.binary_output)
                        out.buffer.flush()
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
                if received_response:
                    # Re-running a partially streamed command would duplicate its output
                    sys.stderr.write(f"gRPC stream failed: {e.code().name}: {e.details()}\n")
                    return 1
                if forward_stdin and e.code() == grpc.StatusCode.UNIMPLEMENTED:
                    forward_stdin = False
                    continue
                if e.code() in RETRYABLE_CODES and attempt < max_retries - 1:
                    delay = base_delay * (2 ** attempt)
                    reason = "Server unavailable" if e.code() == grpc.StatusCode.UNAVAILABLE else "Server busy"
                    sys.stderr.write(f"{reason}, retrying in {delay:.1f} seconds... (Attempt {attempt + 1}/{max_retries})\n")
                    await asyncio.sleep(delay)
                    attempt += 1
                    continue
                sys.stderr.write(f"gRPC error after {attempt + 1} attempts: {e.code().name}: {e.details()}\n")
                return 1
            except Exception as e:
                sys.stderr.write(f"An unexpected error occurred: {e}\n")
                return 1


if __name__ == "__main__":
    # Busybox style: the name this script is invoked as (e.g. a symlink named
    # "ffmpeg" or "ffprobe") is the remote binary to run.
    script_name = os.path.basename(sys.argv[0])
    exit_code = asyncio.run(run_command([script_name] + sys.argv[1:], USE_SSL))
    if exit_code < 0:
        # The remote process was killed by a signal; die the same way so the
        # caller sees the same status as for a local process.
        try:
            signal.signal(-exit_code, signal.SIG_DFL)
        except (OSError, ValueError):
            pass  # SIGKILL/SIGSTOP always have their default action
        os.kill(os.getpid(), -exit_code)
        exit_code = 128 - exit_code
    sys.exit(exit_code)
