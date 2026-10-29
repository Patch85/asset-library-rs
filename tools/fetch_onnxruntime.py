"""Download the official ONNX Runtime build into vendor/onnxruntime/<platform>/.

The app loads ONNX Runtime at runtime (ort's load-dynamic) instead of linking it, because
ort's prebuilt static library needs glibc 2.38+ and Microsoft's builds run on older Linux.
Debug builds find the host platform's folder on their own; installers bundle the target's
folder as a resource (crates/desktop/tauri.<os>.conf.json). Fetch another platform's build to
cross-compile, e.g. --platform win-x64 for the Windows installer.

Usage: python tools/fetch_onnxruntime.py [--platform linux-x64|linux-aarch64|osx-arm64|win-x64]
"""

import argparse
import io
import platform
import shutil
import tarfile
import urllib.request
import zipfile
from pathlib import Path

VERSION = "1.30.0"  # ort 2.0.0-rc.15 supports ONNX Runtime 1.24-1.31
ROOT = Path(__file__).resolve().parent.parent
VENDOR = ROOT / "vendor" / "onnxruntime"


def detect() -> str:
    machine = platform.machine().lower()
    arm = machine in ("arm64", "aarch64")
    system = platform.system()
    if system == "Linux":
        return "linux-aarch64" if arm else "linux-x64"
    if system == "Darwin":
        if not arm:
            raise SystemExit("Microsoft no longer publishes ONNX Runtime for Intel Macs")
        return "osx-arm64"
    if system == "Windows":
        return "win-x64"
    raise SystemExit(f"unsupported platform: {system} {machine}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--platform", default=None)
    args = p.parse_args()
    plat = args.platform or detect()
    ext = "zip" if plat.startswith("win") else "tgz"
    name = f"onnxruntime-{plat}-{VERSION}"
    url = f"https://github.com/microsoft/onnxruntime/releases/download/v{VERSION}/{name}.{ext}"
    out = VENDOR / plat
    print(f"downloading {url}")
    data = urllib.request.urlopen(url).read()

    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    wanted = ("onnxruntime.dll", "onnxruntime_providers_shared.dll", "libonnxruntime.so",
              "libonnxruntime.dylib", "libonnxruntime_providers_shared.so")
    if ext == "zip":
        with zipfile.ZipFile(io.BytesIO(data)) as zf:
            members = [(m, zf.read(m)) for m in zf.namelist() if "/lib/" in m and not m.endswith("/")]
    else:
        with tarfile.open(fileobj=io.BytesIO(data)) as tf:
            members = [(m.name, tf.extractfile(m).read()) for m in tf.getmembers()
                       if "/lib/" in m.name and m.isfile()]
    for path, content in members:
        base = Path(path).name
        # Versioned .so/.dylib files (the plain names are symlinks to them in the archive)
        # are saved under the plain name the app loads.
        if base.startswith("libonnxruntime.so."):
            base = "libonnxruntime.so"
        elif base.startswith("libonnxruntime.1.") and base.endswith(".dylib"):
            base = "libonnxruntime.dylib"
        if base in wanted:
            (out / base).write_bytes(content)
    print("wrote", ", ".join(sorted(f.name for f in out.iterdir())))


if __name__ == "__main__":
    main()
