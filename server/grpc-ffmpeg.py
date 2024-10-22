#!/usr/bin/env python3
import asyncio
import subprocess
from concurrent.futures import ThreadPoolExecutor
import logging
from aiohttp import web
import os
import shlex
import signal
import time
import sys

import grpc
import ffmpeg_pb2
import ffmpeg_pb2_grpc

# Required for MediaInfo and ffmpeg health check
import aiofiles
from pymediainfo import MediaInfo

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Configuration
VALID_TOKEN = os.getenv('VALID_TOKEN', 'my_secret_token')
ALLOWED_BINARIES = ['ffmpeg', 'ffprobe', 'mediainfo']  # Added 'mediainfo' to allowed binaries
BINARY_PATH_PREFIX = os.getenv('BINARY_PATH_PREFIX', '/usr/lib/jellyfin-ffmpeg/')
SSL_KEY_PATH = os.getenv('SSL_KEY_PATH', 'server.key')
SSL_CERT_PATH = os.getenv('SSL_CERT_PATH', 'server.crt')
USE_SSL = os.getenv('USE_SSL', 'false').lower() == 'true'

# Health check variables
HEALTHCHECK_INTERVAL = 60  # Interval in seconds (1 hour)
HEALTHCHECK_FILE = '/app/healthcheck.mkv'
HEALTHCHECK_OUTPUT = '/tmp/healthcheck_output.mp4'

# Variable to store health status
health_status = {'healthy': False}

class TokenAuthValidator(grpc.AuthMetadataPlugin):
    def __call__(self, context, callback):
        token = None
        for key, value in context.invocation_metadata():
            if key == 'authorization':
                token = value
                break

        if token is None or token != VALID_TOKEN:
            callback(grpc.StatusCode.UNAUTHENTICATED, 'Invalid token')
        else:
            callback(None, None)

class FFmpegService(ffmpeg_pb2_grpc.FFmpegServiceServicer):
    async def ExecuteCommand(self, request, context):
        command = request.command
        logger.info(f'Received command: {command}')

        # Tokenize the command
        tokens = shlex.split(command)

        # Check if the command is allowed
        if tokens[0] not in ALLOWED_BINARIES:
            yield ffmpeg_pb2.CommandResponse(output="Error: Command not allowed", exit_code=1)
            return

        # Prepend the binary path prefix
        tokens[0] = os.path.join(BINARY_PATH_PREFIX, tokens[0])

        # Reconstruct the command
        command = shlex.join(tokens)

        process = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE
        )

        async def read_stream(stream, response_type, stream_name):
            while True:
                line = await stream.readline()
                if not line:
                    break
                logger.info(f'{stream_name}: {line.decode("utf-8").strip()}')
                yield response_type(output=line.decode('utf-8'))

        async for response in read_stream(process.stdout, ffmpeg_pb2.CommandResponse, "STDOUT"):
            yield response

        async for response in read_stream(process.stderr, ffmpeg_pb2.CommandResponse, "STDERR"):
            yield response

        await process.wait()
        exit_code = process.returncode
        yield ffmpeg_pb2.CommandResponse(exit_code=exit_code)

    async def health_check(self):
        logger.info("Running initial health check...")
        await self.run_health_check()

        while True:
            await asyncio.sleep(HEALTHCHECK_INTERVAL)
            logger.debug("Running periodic health check...")
            await self.run_health_check()

    async def run_health_check(self):
        # Run mediainfo on health check file
        media_info = await self.run_command(f"mediainfo {HEALTHCHECK_FILE}")
        if "Video" not in media_info:
            logger.error(f"MediaInfo failed for {HEALTHCHECK_FILE}")
            self.update_health_status(False)
            return

        cleanup_command = f"rm -f {HEALTHCHECK_OUTPUT}"
        await self.run_command(cleanup_command)

        # Run ffmpeg conversion test
        ffmpeg_command = f"{BINARY_PATH_PREFIX}ffmpeg -i {HEALTHCHECK_FILE} {HEALTHCHECK_OUTPUT}"
        ffmpeg_output = await self.run_command(ffmpeg_command)
        if "Conversion failed" in ffmpeg_output:
            logger.error("FFmpeg conversion test failed")
            self.update_health_status(False)
            return

        # Check if output file is valid
        if not await self.is_file_valid(HEALTHCHECK_OUTPUT):
            logger.error("Output file is not valid")
            self.update_health_status(False)
            return

        self.update_health_status(True)
        logger.debug("Health check passed successfully")

    def update_health_status(self, is_healthy):
        global health_status
        health_status['healthy'] = is_healthy

    async def run_command(self, command):
        process = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE
        )

        stdout, stderr = await process.communicate()
        output = stdout.decode().strip() if stdout else ""
        error = stderr.decode().strip() if stderr else ""

        if process.returncode != 0:
            logger.error(f"Command '{command}' failed with error: {error}")
            return f"Command '{command}' failed with error: {error}"
        
        return output

    async def is_file_valid(self, filename):
        try:
            media_info = MediaInfo.parse(filename)
            return True  # Example: Check if MediaInfo indicates valid media file
        except Exception as e:
            logger.error(f"Error checking file {filename}: {str(e)}")
            return False

async def start_grpc_server():
    server = grpc.aio.server(ThreadPoolExecutor(max_workers=10))
    ffmpeg_pb2_grpc.add_FFmpegServiceServicer_to_server(FFmpegService(), server)

    listen_addr = '0.0.0.0:50051'
    if USE_SSL:
        with open(SSL_CERT_PATH, 'rb') as f:
            certificate_chain = f.read()
        with open(SSL_KEY_PATH, 'rb') as f:
            private_key = f.read()
        server_creds = grpc.ssl_server_credentials(((private_key, certificate_chain),))
        server.add_secure_port(listen_addr, server_creds)
        logger.info(f'Server started with SSL on {listen_addr}')
    else:
        server.add_insecure_port(listen_addr)
        logger.info(f'Server started without SSL on {listen_addr}')

    await server.start()
    try:
        await server.wait_for_termination()
    except asyncio.CancelledError:
        pass
    finally:
        await server.stop(0)

async def health_check_runner():
    service = FFmpegService()
    await service.health_check()

async def start_http_server():
    async def health_check(request):
        if health_status['healthy']:
            return web.Response(text="OK")
        else:
            return web.Response(text="Health check failed", status=500)

    app = web.Application()
    app.router.add_get('/health', health_check)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, '0.0.0.0', 8080)
    await site.start()
    logger.info('Health check server started on http://localhost:8080/health')

async def ffmpeg_server():
    grpc_task = asyncio.create_task(start_grpc_server())
    http_task = asyncio.create_task(start_http_server())
    health_task = asyncio.create_task(health_check_runner())
    await asyncio.gather(grpc_task, http_task, health_task)

def handle_signals():
    loop = asyncio.get_event_loop()
    for signame in {'SIGINT', 'SIGTERM'}:
        loop.add_signal_handler(getattr(signal, signame), lambda: asyncio.create_task(shutdown(signame)))

async def shutdown(signame):
    logger.info(f"Received signal {signame}, shutting down...")
    tasks = [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)

if __name__ == '__main__':
    handle_signals()
    asyncio.run(ffmpeg_server())
