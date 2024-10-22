#!/usr/bin/env python3
import grpc
import ffmpeg_pb2
import ffmpeg_pb2_grpc
import asyncio
import os
import sys
import shlex

# Configuration
CERTIFICATE_PATH = os.getenv('CERTIFICATE_PATH', 'server.crt')
AUTH_TOKEN = os.getenv('AUTH_TOKEN', 'my_secret_token1')
GRPC_HOST = os.getenv('GRPC_HOST', 'ffmpeg-workers')
GRPC_PORT = os.getenv('GRPC_PORT', '50051')
USE_SSL = os.getenv('USE_SSL', 'false').lower() == 'true'

async def run_command(command, use_ssl):
    target = f"{GRPC_HOST}:{GRPC_PORT}"
    if use_ssl:
        with open(CERTIFICATE_PATH, 'rb') as f:
            trusted_certs = f.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=trusted_certs)
        call_credentials = grpc.metadata_call_credentials(
            lambda context, callback: callback((('authorization', AUTH_TOKEN),), None)
        )
        composite_credentials = grpc.composite_channel_credentials(credentials, call_credentials)
        channel = grpc.aio.secure_channel(target, composite_credentials)
    else:
        channel = grpc.aio.insecure_channel(target)

    async with channel:
        stub = ffmpeg_pb2_grpc.FFmpegServiceStub(channel)
        request = ffmpeg_pb2.CommandRequest(command=command)
        async for response in stub.ExecuteCommand(request):
            if response.output:
                print(response.output, end="")
            if response.exit_code != 0:
                print(f"\nExit code: {response.exit_code}")


if __name__ == '__main__':
    # Determine the name the script was called with
    script_name = os.path.basename(sys.argv[0])
    
    # Capture the arguments as a list
    command = shlex.join(sys.argv[0:])
    
    # Use shlex to reassemble the command string with appropriate quoting
    #command_str = shlex.join(command)  # shlex.join preserves necessary quotes
    
    # Print the command list and the reassembled command string
    print(command)
    #print(command_str)

    # Run the command
    asyncio.run(run_command(command_str, USE_SSL))