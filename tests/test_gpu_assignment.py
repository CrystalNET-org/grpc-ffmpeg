"""Tests for the worker's GPU assignment (CUDA_DEVICES).

Run from the repository root, with the gRPC stubs generated into gen/ (see
CONTRIBUTING.md):
    PYTHONPATH=gen python -m unittest discover tests
"""
import unittest
from unittest import mock

import worker_module

worker = worker_module.load()


class UsesCudaTest(unittest.TestCase):
    def test_jellyfin_nvenc_command(self):
        tokens = "ffmpeg -init_hw_device cuda=cu:0 -filter_hw_device cu -hwaccel cuda -i a.mkv -c:v h264_nvenc o.ts".split()
        self.assertTrue(worker.uses_cuda(tokens))

    def test_cuda_decoding_only(self):
        self.assertTrue(worker.uses_cuda("ffmpeg -hwaccel cuda -i a.mkv o.mp4".split()))

    def test_other_commands(self):
        self.assertFalse(worker.uses_cuda("ffmpeg -version".split()))
        self.assertFalse(worker.uses_cuda("ffmpeg -init_hw_device qsv=qs@va -i a.mkv o.ts".split()))
        self.assertFalse(worker.uses_cuda("ffmpeg -i cuda.mkv o.ts".split()))
        self.assertFalse(worker.uses_cuda(["ffmpeg", "-init_hw_device"]))


class GpuAssignerTest(unittest.TestCase):
    def test_no_devices(self):
        assigner = worker.GpuAssigner([])
        self.assertIsNone(assigner.acquire())
        assigner.release(None)

    def test_takes_turns_when_equally_busy(self):
        assigner = worker.GpuAssigner(["0", "1"])
        first = assigner.acquire()
        assigner.release(first)
        second = assigner.acquire()
        assigner.release(second)
        self.assertEqual((first, second), ("0", "1"))

    def test_least_busy_gpu(self):
        assigner = worker.GpuAssigner(["0", "1", "2"])
        held = [assigner.acquire() for _ in range(3)]
        self.assertEqual(sorted(held), ["0", "1", "2"])
        assigner.release("1")
        self.assertEqual(assigner.acquire(), "1")
        self.assertEqual(assigner.active, {"0": 1, "1": 1, "2": 1})
        # All equally busy again: spread evenly
        more = [assigner.acquire() for _ in range(3)]
        self.assertEqual(sorted(more), ["0", "1", "2"])


class DetectCudaDevicesTest(unittest.TestCase):
    def test_list(self):
        self.assertEqual(worker.detect_cuda_devices(" 0, 1 ,,GPU-abc "), ["0", "1", "GPU-abc"])
        self.assertEqual(worker.detect_cuda_devices(""), [])

    def test_auto(self):
        result = mock.Mock(stdout="0\n1\n")
        with mock.patch.object(worker.subprocess, "run", return_value=result):
            self.assertEqual(worker.detect_cuda_devices("auto"), ["0", "1"])

    def test_auto_without_nvidia_smi(self):
        with mock.patch.object(worker.subprocess, "run", side_effect=FileNotFoundError("nvidia-smi")):
            self.assertEqual(worker.detect_cuda_devices("AUTO"), [])


if __name__ == "__main__":
    unittest.main()
