"""Run the Python original and the Rust port on copies of the real library and diff answers.

Both servers get their own temp data dir holding a copy of the golden library.db and of the
original's thumbnails, so neither has anything to scan, thumbnail or analyse. The original
is only read (copied), and its server runs with PYTHONDONTWRITEBYTECODE=1.

Deterministic endpoints (search, tags, sources, asset details, suggestions, files) must match
exactly. Image search (~"..." and similar:) is reported as top-k overlap, since the Rust
text model output differs slightly.

Usage: python tools/diff_servers.py [--rust target/release/asset-library] [--keep]
Exit status is 1 if any deterministic endpoint differs.
"""

import argparse
import json
import os
import platform
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx

ROOT = Path(__file__).resolve().parent.parent
ORIGINAL = Path(os.environ.get("ASSET_LIBRARY_ORIGINAL", ROOT.parent / "asset-library"))
GOLDEN_DB = ROOT / "fixtures" / "golden" / "library.db"
MODELS = ROOT / "models"

TEXT_QUERIES = ["a red dragon", "skeleton warrior with a sword", "wolf", "a treasure chest",
                "elf archer", "humanoid with an octopus head and face tentacles"]


def wsl_path(p: str) -> str:
    """C:\\Users\\x -> /mnt/c/Users/x, so a Windows-made library resolves under WSL."""
    m = re.match(r"^([A-Za-z]):[\\/](.*)$", p)
    if not m or platform.system() != "Linux":
        return p
    return f"/mnt/{m[1].lower()}/" + m[2].replace("\\", "/")


def make_data_dir(base: Path, name: str) -> Path:
    d = base / name
    (d / "thumbs").parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(GOLDEN_DB, d / "library.db")
    shutil.copytree(ORIGINAL / "data" / "thumbs", d / "thumbs")
    conn = sqlite3.connect(d / "library.db")
    for sid, path in conn.execute("SELECT id, path FROM sources").fetchall():
        conn.execute("UPDATE sources SET path = ? WHERE id = ?", (wsl_path(path), sid))
    conn.commit()
    conn.close()
    return d


def free_port() -> int:
    import socket
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start(cmd: list[str], cwd: Path, data: Path, log: Path) -> tuple[subprocess.Popen, httpx.Client]:
    port = free_port()
    env = os.environ | {"ASSET_LIBRARY_DATA": str(data), "ASSET_LIBRARY_MODELS": str(MODELS),
                        "ASSET_LIBRARY_AI": "1", "PYTHONDONTWRITEBYTECODE": "1"}
    proc = subprocess.Popen([*cmd, "--no-browser", "--port", str(port)], cwd=cwd, env=env,
                            stdout=open(log, "w"), stderr=subprocess.STDOUT)
    client = httpx.Client(base_url=f"http://127.0.0.1:{port}", timeout=120)
    for _ in range(600):
        if proc.poll() is not None:
            raise SystemExit(f"{cmd[0]} exited early:\n{log.read_text()}")
        try:
            if client.get("/api/meta").status_code == 200:
                break
        except httpx.TransportError:
            time.sleep(0.1)
    for _ in range(6000):  # both should be idle at once; nothing is pending
        s = client.get("/api/scan/status").json()
        if not (s["running"] or s["thumbs"]["running"] or s["analysis"]["running"]):
            break
        time.sleep(0.1)
    return proc, client


def queries(db: Path) -> list[str]:
    """Search queries covering every part of the query language, drawn from the library."""
    conn = sqlite3.connect(db)
    tags = [r[0] for r in conn.execute(
        "SELECT t.name FROM tags t JOIN asset_tags x ON x.tag_id = t.id "
        "GROUP BY t.id ORDER BY count(*) DESC LIMIT 12")]
    words = []
    for (name,) in conn.execute("SELECT name FROM assets ORDER BY id LIMIT 4000"):
        for w in name.split():
            if w.isalpha() and len(w) > 3 and w.lower() not in words:
                words.append(w.lower())
    conn.close()
    words = words[::max(1, len(words) // 15)][:15]
    colors = ["red", "orange", "yellow", "green", "teal", "blue", "purple", "pink", "brown",
              "black", "white", "gray"]
    q = ["", "is:fav", "is:untagged", "is:missing", "is:transparent", "-is:transparent",
         "type:token", "type:map", "w>=512", "w<300 h<300", "h=256", "source:1", 'bad "quote',
         '"large construct"', "-tag:undead", "similar:abc"]
    q += [f"tag:{t}" for t in tags] + [f"-tag:{t} {w}" for t, w in zip(tags, words)]
    q += words + [w[:3] for w in words] + [f"-{w}" for w in words[:5]]
    q += [f"color:{c}" for c in colors] + [f"-color:{c} tag:{tags[0]}" for c in colors[:4]]
    q += [f"{w} color:{c}" for w, c in zip(words, colors)]
    return q


def diff(a, b, path="") -> list[str]:
    if type(a) is not type(b):
        if isinstance(a, (int, float)) and isinstance(b, (int, float)) and a == b:
            return []
        return [f"{path}: {a!r} != {b!r}"]
    if isinstance(a, dict):
        out = []
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                out.append(f"{path}.{k}: only in {'rust' if k in b else 'python'}")
            else:
                out += diff(a[k], b[k], f"{path}.{k}")
        return out
    if isinstance(a, list):
        if len(a) != len(b):
            return [f"{path}: length {len(a)} != {len(b)}"]
        return [d for i, (x, y) in enumerate(zip(a, b)) for d in diff(x, y, f"{path}[{i}]")]
    return [] if a == b else [f"{path}: {a!r} != {b!r}"]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--rust", default=str(ROOT / "target" / "release" / "asset-library"))
    p.add_argument("--keep", action="store_true", help="keep the temp data dirs")
    args = p.parse_args()
    if not GOLDEN_DB.exists():
        raise SystemExit("run tools/make_golden.py first")

    base = Path(tempfile.mkdtemp(prefix="diff-servers-"))
    print(f"data dirs under {base}")
    py_data, rs_data = make_data_dir(base, "python"), make_data_dir(base, "rust")
    py, pc = start([sys.executable, str(ORIGINAL / "run.py")], ORIGINAL, py_data, base / "python.log")
    rs, rc = start([args.rust], ROOT, rs_data, base / "rust.log")
    failures, checks = [], 0

    def check(label: str, method: str, url: str, **kw):
        nonlocal checks
        checks += 1
        ra, rb = pc.request(method, url, **kw), rc.request(method, url, **kw)
        if ra.status_code != rb.status_code:
            failures.append(f"{label}: status {ra.status_code} != {rb.status_code} ({rb.text[:200]})")
            return None, None
        ct = ra.headers.get("content-type", "")
        if "json" in ct:
            d = diff(ra.json(), rb.json())
            if d:
                failures.append(f"{label}: " + "; ".join(d[:5]) + (f" (+{len(d) - 5} more)" if len(d) > 5 else ""))
            return ra.json(), rb.json()
        if ra.content != rb.content:
            failures.append(f"{label}: body differs ({len(ra.content)} vs {len(rb.content)} bytes)")
        return None, None

    try:
        for path in ["/api/meta", "/api/sources", "/api/tags", "/api/suggestions/ignored"]:
            check(path, "GET", path)
        qs = queries(py_data / "library.db")
        for q in qs:
            check(f"search {q!r}", "GET", "/api/assets", params={"q": q, "limit": 1000})
        for sort in ["name", "newest", "oldest", "path", "largest", "bogus"]:
            check(f"sort {sort}", "GET", "/api/assets", params={"q": "", "sort": sort, "limit": 300, "offset": 150})

        all_ids = [i["id"] for i in pc.get("/api/assets", params={"limit": 1000}).json()["items"]]
        sample = all_ids[::max(1, len(all_ids) // 60)][:60]
        for aid in sample:
            check(f"asset {aid}", "GET", f"/api/assets/{aid}")
            check(f"suggest {aid}", "POST", "/api/suggestions", json={"ids": [aid]})
        check("suggest merged", "POST", "/api/suggestions", json={"ids": sample[:20]})
        for aid in sample[:5]:
            check(f"thumb {aid}", "GET", f"/api/assets/{aid}/thumb")
            check(f"file {aid}", "GET", f"/api/assets/{aid}/file")
        check("asset 404", "GET", "/api/assets/999999999")

        print("\nimage search (top-k overlap, informational):")
        for q in [f'~"{t}"' for t in TEXT_QUERIES] + [f"similar:{aid}" for aid in sample[:6]]:
            a = [i["id"] for i in pc.get("/api/assets", params={"q": q, "limit": 100}).json()["items"]]
            b = [i["id"] for i in rc.get("/api/assets", params={"q": q, "limit": 100}).json()["items"]]
            print(f"  {q[:60]:<62} top-20 {len(set(a[:20]) & set(b[:20])):>2}/20  "
                  f"top-100 {len(set(a) & set(b)):>3}/{len(a)}")
    finally:
        for proc in (py, rs):
            proc.terminate()
            proc.wait(10)
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)

    print(f"\n{checks - len(failures)}/{checks} deterministic checks identical")
    for f in failures:
        print("  DIFF", f)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
