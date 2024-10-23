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
# Add any params here that need quoting on theyre values
parameters_to_quote = ['-filter_complex', '-vf', '-hls_segment_filename', '-user_agent']

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

def handle_quoted_arguments(command_args):
    """
    Handles the quoting and reassembly of specific arguments like -i file: and -filter_complex.
    - Quotes the file path for the -i file: argument if it contains spaces or special characters.
    - Quotes the entire filter complex string if it contains spaces, commas, or colons.
    """
    rffmpeg_command = []
    i = 0

    while i < len(command_args):
        arg = command_args[i]

        # Handle -i file: argument separately
        if arg == '-i' and i + 1 < len(command_args) and command_args[i + 1].startswith('file:'):
            file_path_arg = command_args[i + 1]
            file_path = file_path_arg[len('file:'):]  # Extract the actual file path

            # Quote the file path if it contains spaces or special characters
            if ' ' in file_path or '(' in file_path or ')' in file_path:
                file_path = f'"{file_path}"'

            # Reassemble the -i file: argument
            rffmpeg_command.append(arg)
            rffmpeg_command.append(f'file:{file_path}')
            i += 2  # Skip the next argument as it's part of -i file:

        # Handle arguments in the parameters_to_quote list
        elif arg in parameters_to_quote and i + 1 < len(command_args):
            next_arg = command_args[i + 1]

            # Quote the argument value if it contains spaces, commas, or colons
            if ' ' in next_arg or ',' in next_arg or ':' in next_arg:
                next_arg = f'"{next_arg}"'

            rffmpeg_command.append(arg)
            rffmpeg_command.append(next_arg)
            i += 2  # Skip the next argument as it's part of this argument set

        # Append any other arguments as is
        else:
            rffmpeg_command.append(arg)
            i += 1

    return rffmpeg_command


if __name__ == '__main__':
    script_name = os.path.basename(sys.argv[0])
    # Get the command line arguments
    command_args = sys.argv[1:]

    # Process and reassemble the arguments
    rffmpeg_command = handle_quoted_arguments(command_args)

    command = [script_name] + rffmpeg_command
    # Convert the list to a single command string
    command_str = ' '.join(command)

    # Run the command
    asyncio.run(run_command(command_str, USE_SSL))