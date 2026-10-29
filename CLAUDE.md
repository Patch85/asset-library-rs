# Asset Library (Rust port)

A Rust + Tauri rewrite of the Python app in `../asset-library`, so friends can install it
without Python. The port reached parity (M0–M6) and now evolves on its own. See `PLAN.md` for
milestones and status.

## Ground rules

- **`../asset-library` is a read-only reference.** Never edit, run scans against, or write
  into it. Read its DB only by copying it first (see `tools/make_golden.py`). Run its server
  only through the parity harness, which points it at a temp data dir and sets
  `PYTHONDONTWRITEBYTECODE=1`.
- **Parity with the Python app is no longer a goal** (decided 2026-10-28). The suite in
  `parity/` is now a regression suite for this app (`PARITY_IMPL=rust`); change its tests
  freely when behavior changes on purpose, and record notable changes in `PLAN.md`. The
  Python original no longer needs to pass them.
- **Never commit real assets.** `fixtures/golden/` holds thumbnails from the user's purchased
  token packs and is gitignored. CI and committed tests use synthetic images only.
- **Keep importing Python libraries working.** The schema may diverge from
  `../asset-library/app/db.py`, but only through `PRAGMA user_version` migrations, which the
  import (`import.rs`) runs on a restored Python database (user_version 0). Thumbnails stay at
  `thumbs/<hash[:2]>/<hash>.webp`, hashes SHA-1, embeddings little-endian f32 L2-normalized.

## Launch contract

```
<server> --no-browser --port N      # serves http://127.0.0.1:N, API under /api
ASSET_LIBRARY_DATA=<dir>            # library.db + thumbs/ live here
ASSET_LIBRARY_MODELS=<dir>          # CLIP model cache
ASSET_LIBRARY_AI=0|1                # 0 = colours only
```

## Commands

```bash
parity/.venv/bin/python tools/fetch_onnxruntime.py     # once per checkout (add --platform win-x64 to cross-build)
PARITY_IMPL=rust parity/.venv/bin/pytest parity        # regression suite against target/debug/asset-library
cargo test --workspace                                 # unit tests + golden comparisons (if fixtures exist)
parity/.venv/bin/python tools/make_golden.py           # regenerate fixtures/golden from ../asset-library
parity/.venv/bin/python tools/diff_servers.py          # historical: Python vs Rust on the real library
cargo run -p asset-library-desktop                     # the Tauri app
tools/build_windows.sh                                 # Windows NSIS installer, cross-built from WSL
target/release/asset-library bench <folder>            # speed and memory on this machine
```

Release builds load ONNX Runtime from next to the executable (or `ORT_DYLIB_PATH`); copy
`vendor/onnxruntime/<platform>/*` there before running `target/release/asset-library` with AI on.
