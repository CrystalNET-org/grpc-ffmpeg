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

CONFIG_FILE_NAME = "grpc-ffmpeg.conf"
DEFAULTS = {
    "GRPC_HOST": "ffmpeg-workers",
    "GRPC_PORT": "50051",
    "USE_SSL": "false",
    "CERTIFICATE_PATH": "server.crt",
    "AUTH_TOKEN": "my_secret_token1",
    "FALLBACK_DIR": "",
    "RETRIES": "5",
}


def config_file(argv0):
    """GRPC_FFMPEG_CONFIG if set, otherwise grpc-ffmpeg.conf in the directory
    the client was invoked from, then in the directory of the script itself."""
    if os.getenv("GRPC_FFMPEG_CONFIG"):
        return os.getenv("GRPC_FFMPEG_CONFIG")
    for directory in (os.path.dirname(os.path.abspath(argv0)),
                      os.path.dirname(os.path.realpath(argv0))):
        path = os.path.join(directory, CONFIG_FILE_NAME)
        if os.path.isfile(path):
            return path
    return None


def parse_config(text):
    """KEY=VALUE lines; blank lines and # comments are ignored, values may be quoted."""
    values = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[key.strip()] = value
    return values


def load_config(argv0):
    """Settings from the config file, overridden by environment variables."""
    config = dict(DEFAULTS)
    path = config_file(argv0)
    if path:
        try:
            with open(path, encoding="utf-8") as f:
                config.update({k: v for k, v in parse_config(f.read()).items() if v})
        except OSError:
            pass
    config.update({k: os.environ[k] for k in DEFAULTS if os.environ.get(k)})
    return config

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


class StdinReader:
    """Reads stdin on a daemon thread (a blocking read cannot be cancelled)
    into a bounded queue shared by all attempts. b"" marks EOF. The thread only
    starts once a request is actually streamed, so nothing is consumed from
    stdin if the command ends up running locally instead."""

    def __init__(self):
        self.queue = None

    def get_queue(self):
        if self.queue is None:
            self.queue = start_stdin_reader(asyncio.get_running_loop())
        return self.queue


def start_stdin_reader(loop):
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


async def execute_requests(request, stdin_reader):
    """Request stream for Execute: the command, then stdin data. Returning
    half-closes the call, which closes the remote process's stdin."""
    yield ffmpeg_pb2.ExecuteRequest(request=request)
    stdin_queue = stdin_reader.get_queue()
    while True:
        data = await stdin_queue.get()
        if not data:
            stdin_queue.put_nowait(b"")  # Keep EOF visible to later attempts
            return
        yield ffmpeg_pb2.ExecuteRequest(stdin=data)


def create_channel(config):
    target = f"{config['GRPC_HOST']}:{config['GRPC_PORT']}"
    if config["USE_SSL"].lower() == "true":
        with open(config["CERTIFICATE_PATH"], "rb") as f:
            trusted_certs = f.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=trusted_certs)
        return grpc.aio.secure_channel(target, credentials, options=CHANNEL_OPTIONS)
    return grpc.aio.insecure_channel(target, options=CHANNEL_OPTIONS)


class Unreachable(Exception):
    """No worker could be reached; the command never started."""


async def run_command(args, config):
    # `command` is for servers that predate the `args` field
    request = ffmpeg_pb2.CommandRequest(
        command=shlex.join(args), args=args, raw_stderr=True
    )
    # Sent as plain metadata so the token also works without SSL
    metadata = (("authorization", config["AUTH_TOKEN"]),)

    stdin_reader = StdinReader()
    # Servers without the Execute RPC do not support stdin forwarding
    forward_stdin = True

    exit_code = 1  # Stays 1 if the server never reports an exit code
    max_retries = max(1, int(config["RETRIES"]))
    base_delay = 1.0
    attempt = 0

    while True:
        received_response = False
        # A new connection per attempt, so behind a load balancer a retry can
        # land on a different worker
        async with create_channel(config) as channel:
            stub = ffmpeg_pb2_grpc.FFmpegServiceStub(channel)
            try:
                if forward_stdin:
                    call = stub.Execute(
                        execute_requests(request, stdin_reader), metadata=metadata
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
                if e.code() == grpc.StatusCode.UNAVAILABLE and config["FALLBACK_DIR"]:
                    raise Unreachable() from e
                sys.stderr.write(f"gRPC error after {attempt + 1} attempts: {e.code().name}: {e.details()}\n")
                return 1
            except Exception as e:
                sys.stderr.write(f"An unexpected error occurred: {e}\n")
                return 1


def run_locally(directory, name, args):
    """Run the local binary of the same name. On Unix the process is replaced,
    so stdin, output, signals and exit status behave exactly as for a local run."""
    path = os.path.join(directory, name)
    if os.name == "nt" and not os.path.isfile(path):
        path += ".exe"
    if not os.path.isfile(path):
        sys.stderr.write(f"No worker reachable and no local fallback at {path}\n")
        return 1
    sys.stderr.write(f"No worker reachable, running {path} locally\n")
    sys.stderr.flush()
    if os.name == "nt":
        import subprocess
        return subprocess.call([path] + args)
    os.execv(path, [path] + args)


if __name__ == "__main__":
    # Busybox style: the name this script is invoked as (e.g. a symlink named
    # "ffmpeg" or "ffprobe") is the remote binary to run.
    script_name = os.path.splitext(os.path.basename(sys.argv[0]))[0]
    config = load_config(sys.argv[0])
    try:
        exit_code = asyncio.run(run_command([script_name] + sys.argv[1:], config))
    except Unreachable:
        exit_code = run_locally(config["FALLBACK_DIR"], script_name, sys.argv[1:])
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
