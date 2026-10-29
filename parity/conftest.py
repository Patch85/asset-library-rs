"""Black-box harness: run an Asset Library server as a subprocess and test it over HTTP.

PARITY_IMPL=rust (default) runs this app; PARITY_IMPL=python runs the original in
../asset-library, which only the tests from before the port diverged (M0-M6) apply to.
Tests may also touch the on-disk format (library.db, thumbs/).
"""

import os
import socket
import sqlite3
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path

import httpx
import numpy as np
import pytest
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ORIGINAL = Path(os.environ.get("ASSET_LIBRARY_ORIGINAL", ROOT.parent / "asset-library"))
MODELS = Path(os.environ.get("PARITY_MODELS", ROOT / "models"))
IMPL = os.environ.get("PARITY_IMPL", "rust")
rust_only = pytest.mark.skipif(IMPL != "rust", reason="feature added after the port diverged")
RUST_BIN = Path(os.environ.get("PARITY_RUST_BIN", ROOT / "target" / "debug" / "asset-library"))
IMAGE_MODEL = "Qdrant/clip-ViT-B-32-vision"
EMBED_DIM = 512


def models_available() -> bool:
    return MODELS.is_dir() and any(MODELS.iterdir())


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, data_dir: Path, ai: bool = False):
        self.data_dir = data_dir
        self.ai = ai
        self.proc: subprocess.Popen | None = None
        self.client: httpx.Client | None = None
        self.log_path = data_dir.parent / f"server-{IMPL}.log"

    @property
    def db_path(self) -> Path:
        return self.data_dir / "library.db"

    def thumb_path(self, file_hash: str) -> Path:
        return self.data_dir / "thumbs" / file_hash[:2] / f"{file_hash}.webp"

    def _command(self, port: int) -> tuple[list[str], Path]:
        if IMPL == "python":
            return [sys.executable, str(ORIGINAL / "run.py"), "--no-browser", "--port", str(port)], ORIGINAL
        if IMPL == "rust":
            if not RUST_BIN.exists():
                pytest.fail(f"Rust binary not built: {RUST_BIN}")
            return [str(RUST_BIN), "--no-browser", "--port", str(port)], ROOT
        raise ValueError(f"PARITY_IMPL must be python or rust, not {IMPL!r}")

    def start(self) -> "Server":
        port = free_port()
        cmd, cwd = self._command(port)
        env = os.environ | {
            "ASSET_LIBRARY_DATA": str(self.data_dir),
            "ASSET_LIBRARY_MODELS": str(MODELS),
            "ASSET_LIBRARY_AI": "1" if self.ai else "0",
            "PYTHONDONTWRITEBYTECODE": "1",  # keep the original's tree untouched
        }
        log = open(self.log_path, "a")
        self.proc = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT)
        self.client = httpx.Client(base_url=f"http://127.0.0.1:{port}", timeout=60)
        for _ in range(300):
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited early:\n{self.log_path.read_text()}")
            try:
                if self.client.get("/api/meta").status_code == 200:
                    return self
            except httpx.TransportError:
                pass
            time.sleep(0.1)
        self.stop()
        raise TimeoutError(f"server did not start:\n{self.log_path.read_text()}")

    def stop(self) -> None:
        if self.client:
            self.client.close()
            self.client = None
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.proc = None

    def restart(self, ai: bool | None = None) -> "Server":
        self.stop()
        if ai is not None:
            self.ai = ai
        return self.start()

    @contextmanager
    def db(self):
        """Direct access to library.db. Prefer stopping the server first so caches reset."""
        conn = sqlite3.connect(self.db_path, timeout=10)
        try:
            yield conn
            conn.commit()
        finally:
            conn.close()

    def inject_embeddings(self, vectors: dict[str, list[float]]) -> None:
        """Give named files fake unit-vector embeddings, then restart so the index rebuilds.
        Tagged with the real model name so an AI-enabled server keeps them."""
        self.stop()
        with self.db() as conn:
            for fname, vec in vectors.items():
                v = np.zeros(EMBED_DIM, dtype=np.float32)
                v[:len(vec)] = vec
                v /= np.linalg.norm(v)
                h = conn.execute("SELECT file_hash FROM assets WHERE rel_path LIKE ?",
                                 (f"%/{fname}",)).fetchone()[0]
                conn.execute("UPDATE image_features SET embedding = ?, embed_model = ? "
                             "WHERE file_hash = ?", (v.astype("<f4").tobytes(), IMAGE_MODEL, h))
        self.start()


@pytest.fixture
def make_server(tmp_path):
    servers = []

    def make(ai: bool = False) -> Server:
        s = Server(tmp_path / "data", ai=ai).start()
        servers.append(s)
        return s

    yield make
    for s in servers:
        s.stop()


def make_img(path: Path, color, size=(64, 64)) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.new("RGBA", size, color).save(path)


def wait_idle(client: httpx.Client) -> dict:
    """Wait until scanning, thumbnailing and analysis have all finished."""
    for _ in range(600):
        s = client.get("/api/scan/status").json()
        if not (s["running"] or s["thumbs"]["running"] or s["analysis"]["running"]):
            return s
        time.sleep(0.05)
    raise TimeoutError


def search(client: httpx.Client, q: str, **kw) -> dict:
    return client.get("/api/assets", params={"q": q, **kw}).json()


def names(result: dict) -> list[str]:
    return sorted(i["name"] for i in result["items"])
