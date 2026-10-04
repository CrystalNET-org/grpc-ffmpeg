"""Tests for how the worker runs commands.

Run from the repository root, with the gRPC stubs generated into gen/ (see
CONTRIBUTING.md):
    PYTHONPATH=gen python -m unittest discover tests
"""
import asyncio
import os
import tempfile
import unittest
from unittest import mock

import worker_module

worker = worker_module.load()


class FakeContext:
    """Collects what the worker writes to the client."""

    def __init__(self):
        self.responses = []

    async def write(self, response):
        self.responses.append(response)

    async def abort(self, code, details):
        raise AssertionError(f"aborted: {code} {details}")


def executable(directory, name, script):
    path = os.path.join(directory, name)
    with open(path, "w") as f:
        f.write("#!/bin/sh\n" + script + "\n")
    os.chmod(path, 0o755)
    return path


class BinaryPathTest(unittest.TestCase):
    def setUp(self):
        worker.binary_path.cache_clear()
        self.addCleanup(worker.binary_path.cache_clear)

    def test_prefers_the_prefix_directory(self):
        with tempfile.TemporaryDirectory() as prefix, tempfile.TemporaryDirectory() as bin_dir:
            executable(bin_dir, "mediainfo", "exit 0")
            in_prefix = executable(prefix, "ffmpeg", "exit 0")
            with mock.patch.object(worker, "BINARY_PATH_PREFIX", prefix), \
                    mock.patch.dict(os.environ, {"PATH": bin_dir}):
                self.assertEqual(worker.binary_path("ffmpeg"), in_prefix)
                # Not part of jellyfin-ffmpeg: from PATH, like /usr/bin/mediainfo in the image
                self.assertEqual(worker.binary_path("mediainfo"), os.path.join(bin_dir, "mediainfo"))
                # Nowhere: the prefix path, so the error names where it was expected
                self.assertEqual(worker.binary_path("vainfo"), os.path.join(prefix, "vainfo"))


class NeedsSlotTest(unittest.TestCase):
    def test_transcodes_and_extractions(self):
        self.assertTrue(worker.needs_slot("ffmpeg", "ffmpeg -i a.mkv -c:v libx264 o.ts".split()))
        self.assertTrue(worker.needs_slot("ffmpeg", "ffmpeg -ss 10 -i a.mkv -frames:v 1 o.jpg".split()))

    def test_queries_and_other_binaries(self):
        self.assertFalse(worker.needs_slot("ffmpeg", "ffmpeg -version".split()))
        self.assertFalse(worker.needs_slot("ffmpeg", "ffmpeg -hide_banner -encoders".split()))
        self.assertFalse(worker.needs_slot("ffprobe", "ffprobe -i a.mkv".split()))


class ListenHostTest(unittest.TestCase):
    def test_ipv4_only_hosts(self):
        with mock.patch.object(worker.socket, "has_ipv6", False):
            self.assertEqual(worker.listen_host(), "0.0.0.0")
        with mock.patch.object(worker.socket, "socket", side_effect=OSError(97, "Address family not supported")):
            self.assertEqual(worker.listen_host(), "0.0.0.0")

    def test_ipv6_hosts(self):
        probe = mock.MagicMock()
        with mock.patch.object(worker.socket, "has_ipv6", True), \
                mock.patch.object(worker.socket, "socket", return_value=probe):
            self.assertEqual(worker.listen_host(), "[::]")


class RunTest(unittest.TestCase):
    def setUp(self):
        worker.binary_path.cache_clear()
        self.addCleanup(worker.binary_path.cache_clear)
        self.prefix = tempfile.TemporaryDirectory()
        self.addCleanup(self.prefix.cleanup)
        patcher = mock.patch.object(worker, "BINARY_PATH_PREFIX", self.prefix.name)
        patcher.start()
        self.addCleanup(patcher.stop)

    def run_command(self, args):
        context = FakeContext()
        request = worker.ffmpeg_pb2.CommandRequest(args=args, raw_stderr=True)
        asyncio.run(worker.FFmpegService().run(request, context, None))
        return context.responses

    def test_reports_start_before_any_output(self):
        executable(self.prefix.name, "ffprobe", "echo out; echo err >&2; exit 3")
        responses = self.run_command(["ffprobe", "-version"])
        self.assertEqual(responses[0].stream, "started")
        self.assertFalse(responses[0].binary_output or responses[0].output)
        self.assertEqual(b"".join(r.binary_output for r in responses if r.stream == "stdout"), b"out\n")
        self.assertEqual(b"".join(r.binary_output for r in responses if r.stream == "stderr"), b"err\n")
        self.assertEqual((responses[-1].stream, responses[-1].exit_code), ("exit_code", 3))

    def test_no_start_for_a_missing_binary(self):
        with tempfile.TemporaryDirectory() as empty, mock.patch.dict(os.environ, {"PATH": empty}):
            responses = self.run_command(["ffprobe", "-version"])
        self.assertNotIn("started", [r.stream for r in responses])
        self.assertEqual((responses[-1].stream, responses[-1].exit_code), ("exit_code", 127))

    def test_queries_do_not_wait_for_a_slot(self):
        executable(self.prefix.name, "ffmpeg", "echo ffmpeg version 8")
        acquire = mock.AsyncMock(return_value=True)
        with mock.patch.object(worker, "acquire_ffmpeg_slot", acquire):
            responses = self.run_command(["ffmpeg", "-version"])
            self.assertEqual(responses[-1].exit_code, 0)
            acquire.assert_not_called()
            # A transcode does wait for one
            self.run_command(["ffmpeg", "-i", "a.mkv", "o.ts"])
            acquire.assert_called_once()


if __name__ == "__main__":
    unittest.main()
