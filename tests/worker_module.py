"""Loads the worker (src/server/grpc-ffmpeg.py) once for all tests: its
Prometheus metrics can only be registered once per process."""
import importlib.util
import pathlib
import sys

SERVER_DIR = pathlib.Path(__file__).resolve().parent.parent / "src" / "server"


def load():
    if "worker" not in sys.modules:
        sys.path.insert(0, str(SERVER_DIR))
        spec = importlib.util.spec_from_file_location("worker", SERVER_DIR / "grpc-ffmpeg.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        sys.modules["worker"] = module
    return sys.modules["worker"]
