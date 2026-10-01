#!/usr/bin/env python3
import asyncio
import codecs
import hmac
import logging
import os
import shlex
import signal
import socket
import sys
import tempfile

import grpc
from aiohttp import web
from prometheus_client import Gauge, Counter, generate_latest, CONTENT_TYPE_LATEST

# Required for the MediaInfo based health check
from pymediainfo import MediaInfo

import ffmpeg_pb2
import ffmpeg_pb2_grpc

logging.basicConfig(
    level=os.getenv("LOG_LEVEL", "INFO").upper(),
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
logger = logging.getLogger("grpc-ffmpeg")


def env_int(name, default):
    value = os.getenv(name)
    if value is None or value == "":
        return default
    try:
        return int(value)
    except ValueError:
        logger.warning(f"Invalid integer for {name}={value!r}, using {default}")
        return default


# Configuration
# Token auth is enforced as soon as VALID_TOKEN is set. Clients may send either
# "<token>" or "Bearer <token>" in the "authorization" metadata.
VALID_TOKEN = os.getenv("VALID_TOKEN", "")
ALLOWED_BINARIES = [
    "ffmpeg",
    "ffprobe",
    "mediainfo",
    "vainfo",
]
BINARY_PATH_PREFIX = os.getenv("BINARY_PATH_PREFIX", "/usr/lib/jellyfin-ffmpeg/")
SSL_KEY_PATH = os.getenv("SSL_KEY_PATH", "server.key")
SSL_CERT_PATH = os.getenv("SSL_CERT_PATH", "server.crt")
USE_SSL = os.getenv("USE_SSL", "false").lower() == "true"
GRPC_PORT = env_int("GRPC_PORT", 50051)
HTTP_PORT = env_int("HTTP_PORT", 8080)
# Max concurrent ffmpeg processes (ffprobe etc. are not limited); further
# ffmpeg calls wait for a free slot. 0 disables the limit.
MAX_FFMPEG_WORKERS = env_int("MAX_FFMPEG_WORKERS", 10)
# Seconds an ffmpeg call may wait for a slot before it is rejected with
# RESOURCE_EXHAUSTED (clients then retry, possibly on another worker).
# 0 waits indefinitely.
FFMPEG_QUEUE_TIMEOUT = env_int("FFMPEG_QUEUE_TIMEOUT", 0)
# How long to wait for in-flight commands on shutdown before cancelling them
SHUTDOWN_GRACE_PERIOD = env_int("SHUTDOWN_GRACE_PERIOD", 5)
# How long a process gets to exit after SIGTERM before it is SIGKILLed
PROCESS_TERM_TIMEOUT = 3
# Read size for subprocess pipes; also the max payload of a streamed message
STREAM_CHUNK_SIZE = 64 * 1024
# Max number of pending output messages per call before we stop reading the pipes
STREAM_QUEUE_SIZE = 64

# Health check variables
HEALTHCHECK_INTERVAL = env_int("HEALTHCHECK_INTERVAL", 60)
HEALTHCHECK_TIMEOUT = env_int("HEALTHCHECK_TIMEOUT", 60)
HEALTHCHECK_FILE = os.getenv(
    "HEALTHCHECK_FILE",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "healthcheck.mkv"),
)
# Unique per worker, in case several workers share a temp directory
HEALTHCHECK_OUTPUT = os.path.join(
    tempfile.gettempdir(),
    f"grpc-ffmpeg-healthcheck-{socket.gethostname()}-{os.getpid()}.mp4",
)

# Detect dead clients so their ffmpeg processes get cleaned up, keep idle
# streams alive through NAT/load balancers, and allow client keepalive pings.
GRPC_SERVER_OPTIONS = [
    ("grpc.keepalive_time_ms", 30_000),
    ("grpc.keepalive_timeout_ms", 10_000),
    ("grpc.keepalive_permit_without_calls", 1),
    ("grpc.http2.max_pings_without_data", 0),
    ("grpc.http2.min_recv_ping_interval_without_data_ms", 10_000),
    ("grpc.http2.max_ping_strikes", 0),
]

# Variable to store health status
health_status = {"healthy": False}

# Background process cleanups that must outlive a cancelled RPC
_cleanup_tasks = set()

ffmpeg_slots = asyncio.Semaphore(MAX_FFMPEG_WORKERS) if MAX_FFMPEG_WORKERS > 0 else None

# Prometheus metrics
binary_counters = {
    binary: Counter(f"{binary}_commands", f"Number of {binary} commands executed")
    for binary in ALLOWED_BINARIES
}
ffmpeg_process_gauge = Gauge(
    "ffmpeg_process_count", "Number of running ffmpeg processes"
)
ffmpeg_max_workers_gauge = Gauge(
    "ffmpeg_max_workers",
    "Maximum number of concurrent ffmpeg processes (0 = unlimited)",
)
ffmpeg_queued_gauge = Gauge(
    "ffmpeg_queued_count", "Number of ffmpeg commands waiting for a free worker slot"
)
ffmpeg_rejected_counter = Counter(
    "ffmpeg_rejected_commands",
    "Number of ffmpeg commands rejected after waiting FFMPEG_QUEUE_TIMEOUT for a slot",
)


def is_authorized(context):
    if not VALID_TOKEN:
        return True
    for key, value in context.invocation_metadata() or ():
        if key != "authorization":
            continue
        if value.startswith("Bearer "):
            value = value[len("Bearer "):]
        if hmac.compare_digest(value.encode(), VALID_TOKEN.encode()):
            return True
        logger.warning(f"Rejected call from {context.peer()}: wrong token")
        return False
    logger.warning(f"Rejected call from {context.peer()}: no token sent")
    return False


def rejoin_split_input_paths(tokens):
    """Heuristic for legacy clients that sent unquoted paths: rejoin the tokens
    following -i up to the next flag."""
    new_tokens = []
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token == "-i" and i + 1 < len(tokens):
            new_tokens.append(token)
            i += 1
            path_parts = []
            while i < len(tokens) and (not tokens[i].startswith("-") or tokens[i] == "-"):
                path_parts.append(tokens[i])
                i += 1
            if path_parts:
                new_tokens.append(" ".join(path_parts))
        else:
            new_tokens.append(token)
            i += 1
    return new_tokens


def parse_request(request):
    """Return the argv for a request. Raises ValueError on malformed input."""
    if request.args:
        return list(request.args)
    # Legacy clients only send a single command string
    return rejoin_split_input_paths(shlex.split(request.command))


async def stop_process(process):
    """Terminate a process, escalating to SIGKILL if it does not exit in time."""
    if process.returncode is not None:
        return
    try:
        process.terminate()
        await asyncio.wait_for(process.wait(), PROCESS_TERM_TIMEOUT)
    except ProcessLookupError:
        pass
    except asyncio.TimeoutError:
        logger.warning(f"Process {process.pid} ignored SIGTERM, killing it")
        try:
            process.kill()
        except ProcessLookupError:
            pass
        await process.wait()


def stop_process_detached(process):
    """Schedule stop_process so it completes even if the caller is cancelled."""
    task = asyncio.ensure_future(stop_process(process))
    _cleanup_tasks.add(task)
    task.add_done_callback(_cleanup_tasks.discard)
    return task


async def pump_stream(stream, stream_name, queue, raw):
    """Forward a subprocess pipe to the response queue in chunks.

    Raw streams are forwarded byte for byte in binary_output. Otherwise (stderr
    for clients that only understand text) the output is decoded incrementally
    so multi-byte characters split across reads are not mangled.
    """
    decoder = None
    if not raw:
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    try:
        while True:
            chunk = await stream.read(STREAM_CHUNK_SIZE)
            if decoder is None:
                if not chunk:
                    break
                logger.debug(f"{stream_name}: {chunk!r}")
                await queue.put(
                    ffmpeg_pb2.CommandResponse(binary_output=chunk, stream=stream_name)
                )
                continue
            text = decoder.decode(chunk, final=not chunk)
            if text:
                logger.debug(f"{stream_name}: {text.rstrip()}")
                await queue.put(ffmpeg_pb2.CommandResponse(output=text, stream=stream_name))
            if not chunk:
                break
    except Exception:
        logger.exception(f"Error while reading {stream_name}")
    # Signal end of this stream
    await queue.put(None)


async def forward_stdin(messages, stdin):
    """Write the stdin data of the remaining request messages to the process,
    closing its stdin once the client half-closes."""
    try:
        async for message in messages:
            if message.stdin:
                stdin.write(message.stdin)
                await stdin.drain()
    except (BrokenPipeError, ConnectionResetError):
        # The process closed its stdin or exited; nothing left to deliver
        return
    except Exception as e:
        # The call failed or was cancelled; the main handler cleans up
        logger.debug(f"Stopped forwarding stdin: {e!r}")
        return
    try:
        stdin.close()
    except (BrokenPipeError, ConnectionResetError):
        pass


async def acquire_ffmpeg_slot(context):
    if ffmpeg_slots is None:
        return False
    if ffmpeg_slots.locked():
        logger.info(f"All {MAX_FFMPEG_WORKERS} ffmpeg slots busy, queueing command")
    ffmpeg_queued_gauge.inc()
    try:
        if FFMPEG_QUEUE_TIMEOUT > 0:
            await asyncio.wait_for(ffmpeg_slots.acquire(), FFMPEG_QUEUE_TIMEOUT)
        else:
            await ffmpeg_slots.acquire()
    except asyncio.TimeoutError:
        ffmpeg_rejected_counter.inc()
        logger.warning(f"No ffmpeg slot free after {FFMPEG_QUEUE_TIMEOUT}s, rejecting command")
        await context.abort(
            grpc.StatusCode.RESOURCE_EXHAUSTED,
            f"All {MAX_FFMPEG_WORKERS} ffmpeg workers are busy",
        )
    finally:
        ffmpeg_queued_gauge.dec()
    return True


class FFmpegService(ffmpeg_pb2_grpc.FFmpegServiceServicer):
    async def ExecuteCommand(self, request, context):
        if not is_authorized(context):
            await context.abort(grpc.StatusCode.UNAUTHENTICATED, "Invalid token")
        await self.run(request, context, None)

    async def Execute(self, request_iterator, context):
        if not is_authorized(context):
            await context.abort(grpc.StatusCode.UNAUTHENTICATED, "Invalid token")
        messages = request_iterator.__aiter__()
        first = await anext(messages, None)
        if first is None or not first.HasField("request"):
            await context.abort(
                grpc.StatusCode.INVALID_ARGUMENT, "The first message must contain the request"
            )
        await self.run(first.request, context, messages)

    async def run(self, request, context, stdin_messages):
        """Run a request. stdin_messages is an async iterator of ExecuteRequest
        whose stdin data is forwarded to the process, or None for no stdin."""
        try:
            tokens = parse_request(request)
        except ValueError as e:
            logger.warning(f"Rejecting malformed command {request.command!r}: {e}")
            await context.write(
                ffmpeg_pb2.CommandResponse(
                    output=f"Error: Malformed command: {e}\n", stream="stderr"
                )
            )
            await context.write(ffmpeg_pb2.CommandResponse(exit_code=1, stream="exit_code"))
            return

        # Check if the command is allowed
        if not tokens or tokens[0] not in ALLOWED_BINARIES:
            logger.warning(f"Rejecting disallowed command: {shlex.join(tokens)}")
            await context.write(
                ffmpeg_pb2.CommandResponse(
                    output="Error: Command not allowed\n", stream="stderr"
                )
            )
            await context.write(ffmpeg_pb2.CommandResponse(exit_code=1, stream="exit_code"))
            return

        binary = tokens[0]
        tokens[0] = os.path.join(BINARY_PATH_PREFIX, binary)
        logger.info(f"Received command: {shlex.join(tokens)}")
        binary_counters[binary].inc()

        is_ffmpeg = binary == "ffmpeg"
        holds_slot = is_ffmpeg and await acquire_ffmpeg_slot(context)
        if is_ffmpeg:
            ffmpeg_process_gauge.inc()
        process = None
        pumps = []
        stdin_forwarder = None
        try:
            try:
                process = await asyncio.create_subprocess_exec(
                    *tokens,
                    stdin=(
                        asyncio.subprocess.DEVNULL
                        if stdin_messages is None
                        else asyncio.subprocess.PIPE
                    ),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
            except OSError as e:
                logger.error(f"Failed to start {tokens[0]}: {e}")
                await context.write(
                    ffmpeg_pb2.CommandResponse(
                        output=f"Error: Failed to start {binary}: {e}\n", stream="stderr"
                    )
                )
                await context.write(
                    ffmpeg_pb2.CommandResponse(exit_code=127, stream="exit_code")
                )
                return

            # Read stdout and stderr concurrently: reading them one after the
            # other deadlocks once the unread pipe fills up, and would delay
            # ffmpeg's progress output (stderr) until stdout is closed.
            queue = asyncio.Queue(maxsize=STREAM_QUEUE_SIZE)
            pumps = [
                asyncio.create_task(pump_stream(process.stdout, "stdout", queue, True)),
                asyncio.create_task(
                    pump_stream(process.stderr, "stderr", queue, request.raw_stderr)
                ),
            ]
            if stdin_messages is not None:
                stdin_forwarder = asyncio.create_task(
                    forward_stdin(stdin_messages, process.stdin)
                )
            open_streams = len(pumps)
            while open_streams:
                response = await queue.get()
                if response is None:
                    open_streams -= 1
                    continue
                await context.write(response)

            exit_code = await process.wait()
            logger.info(f"{binary} (pid {process.pid}) exited with code {exit_code}")
            await context.write(
                ffmpeg_pb2.CommandResponse(exit_code=exit_code, stream="exit_code")
            )
        except asyncio.CancelledError:
            logger.info(f"Call cancelled, stopping {binary} (pid {process.pid if process else '-'})")
            raise
        finally:
            for pump in pumps:
                pump.cancel()
            if stdin_forwarder is not None:
                stdin_forwarder.cancel()
            if process is not None and process.returncode is None:
                # Shield the cleanup so a repeated cancellation cannot leave
                # an orphaned ffmpeg process behind.
                await asyncio.shield(stop_process_detached(process))
            if is_ffmpeg:
                ffmpeg_process_gauge.dec()
            if holds_slot:
                ffmpeg_slots.release()


class HealthChecker:
    async def run(self):
        logger.info("Running initial health check...")
        while True:
            try:
                healthy = await asyncio.wait_for(self.check(), HEALTHCHECK_TIMEOUT)
            except asyncio.TimeoutError:
                logger.error(f"Health check timed out after {HEALTHCHECK_TIMEOUT}s")
                healthy = False
            except Exception:
                logger.exception("Health check crashed")
                healthy = False
            if healthy != health_status["healthy"]:
                logger.info(f"Health status changed to {'healthy' if healthy else 'unhealthy'}")
            health_status["healthy"] = healthy
            await asyncio.sleep(HEALTHCHECK_INTERVAL)

    async def check(self):
        # Run mediainfo on health check file
        returncode, media_info = await self.run_command(
            ["mediainfo", HEALTHCHECK_FILE]
        )
        if returncode != 0 or "Video" not in media_info:
            logger.error(f"MediaInfo failed for {HEALTHCHECK_FILE}")
            return False

        try:
            os.remove(HEALTHCHECK_OUTPUT)
        except FileNotFoundError:
            pass

        # Run ffmpeg conversion test
        returncode, _ = await self.run_command(
            [
                os.path.join(BINARY_PATH_PREFIX, "ffmpeg"),
                "-nostdin",
                "-hide_banner",
                "-loglevel", "error",
                "-y",
                "-i", HEALTHCHECK_FILE,
                HEALTHCHECK_OUTPUT,
            ]
        )
        if returncode != 0:
            logger.error("FFmpeg conversion test failed")
            return False

        # Check if output file is valid
        if not await asyncio.to_thread(self.is_file_valid, HEALTHCHECK_OUTPUT):
            logger.error("Output file is not valid")
            return False

        logger.debug("Health check passed successfully")
        return True

    async def run_command(self, args):
        process = await asyncio.create_subprocess_exec(
            *args,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            stdout, stderr = await process.communicate()
        finally:
            # Only reached with a live process if the check timed out
            if process.returncode is None:
                await asyncio.shield(stop_process_detached(process))

        if process.returncode != 0:
            error = stderr.decode("utf-8", errors="replace").strip()
            logger.error(f"Command '{shlex.join(args)}' failed with error: {error}")
        return process.returncode, stdout.decode("utf-8", errors="replace")

    @staticmethod
    def is_file_valid(filename):
        try:
            media_info = MediaInfo.parse(filename)
        except Exception as e:
            logger.error(f"Error checking file {filename}: {e}")
            return False
        return any(track.track_type == "Video" for track in media_info.tracks)


async def start_grpc_server():
    server = grpc.aio.server(options=GRPC_SERVER_OPTIONS)
    ffmpeg_pb2_grpc.add_FFmpegServiceServicer_to_server(FFmpegService(), server)

    ffmpeg_max_workers_gauge.set(MAX_FFMPEG_WORKERS)

    listen_addr = f"0.0.0.0:{GRPC_PORT}"
    if USE_SSL:
        with open(SSL_CERT_PATH, "rb") as f:
            certificate_chain = f.read()
        with open(SSL_KEY_PATH, "rb") as f:
            private_key = f.read()
        server_creds = grpc.ssl_server_credentials(((private_key, certificate_chain),))
        server.add_secure_port(listen_addr, server_creds)
        logger.info(f"Server started with SSL on {listen_addr}")
    else:
        server.add_insecure_port(listen_addr)
        logger.info(f"Server started without SSL on {listen_addr}")

    if VALID_TOKEN:
        logger.info("Token authentication enabled")
    else:
        logger.warning("VALID_TOKEN is not set, token authentication is disabled")

    await server.start()
    return server


async def start_http_server():
    async def health_check(request):
        if health_status["healthy"]:
            return web.Response(text="OK")
        return web.Response(text="Health check failed", status=500)

    async def metrics(request):
        return web.Response(
            body=generate_latest(), headers={"Content-Type": CONTENT_TYPE_LATEST}
        )

    app = web.Application()
    app.router.add_get("/health", health_check)
    app.router.add_get("/metrics", metrics)  # Prometheus metrics endpoint

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "0.0.0.0", HTTP_PORT)
    await site.start()
    logger.info(
        f"http endpoint server started on http://localhost:{HTTP_PORT} /health and /metrics"
    )
    return runner


async def main():
    loop = asyncio.get_running_loop()
    stop_event = asyncio.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop_event.set)

    grpc_server = await start_grpc_server()
    http_runner = await start_http_server()
    health_task = asyncio.create_task(HealthChecker().run())

    await stop_event.wait()
    logger.info("Received shutdown signal, shutting down...")

    health_task.cancel()
    # Stop accepting new calls; in-flight calls get a grace period, after
    # which they are cancelled and their processes terminated.
    await grpc_server.stop(SHUTDOWN_GRACE_PERIOD)
    if _cleanup_tasks:
        await asyncio.wait(_cleanup_tasks, timeout=PROCESS_TERM_TIMEOUT + 1)
    await http_runner.cleanup()
    await asyncio.gather(health_task, return_exceptions=True)
    logger.info("Shutdown complete.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception:
        logger.exception("Unhandled exception")
        sys.exit(1)
