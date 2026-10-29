"""Download the official ONNX Runtime build for this platform into vendor/onnxruntime/lib.

The app loads ONNX Runtime at runtime (ort's load-dynamic) instead of linking it, because
ort's prebuilt static library needs glibc 2.38+ and Microsoft's builds run on older Linux.
Debug builds find vendor/onnxruntime/lib on their own; release builds and installers ship
the library next to the executable.

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
OUT = ROOT / "vendor" / "onnxruntime" / "lib"


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
    print(f"downloading {url}")
    data = urllib.request.urlopen(url).read()

    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
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
        # Keep the plain names (and versioned .so/.dylib files the plain names point to).
        if base in wanted or base.startswith(("libonnxruntime.so.", "libonnxruntime.1.")):
            (OUT / base).write_bytes(content)
    # tar symlinks don't come through extractfile(); provide the plain name the app looks for.
    for plain, prefix in (("libonnxruntime.so", "libonnxruntime.so."),
                          ("libonnxruntime.dylib", "libonnxruntime.1.")):
        versioned = sorted(OUT.glob(prefix + "*"))
        if versioned and not (OUT / plain).exists():
            shutil.copy2(versioned[-1], OUT / plain)
    print("wrote", ", ".join(sorted(f.name for f in OUT.iterdir())))


if __name__ == "__main__":
    main()
