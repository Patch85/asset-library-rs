# Asset Library (Rust port)

A Rust + Tauri rewrite of the Python app in `../asset-library`, so friends can install it
without Python. See `PLAN.md` for milestones and status.

## Ground rules

- **`../asset-library` is a read-only reference.** Never edit, run scans against, or write
  into it. Read its DB only by copying it first (see `tools/make_golden.py`). Run its server
  only through the parity harness, which points it at a temp data dir and sets
  `PYTHONDONTWRITEBYTECODE=1`.
- **The parity suite in `parity/` is the spec.** A feature is ported when its parity tests pass
  with `PARITY_IMPL=rust`. Change a test only if the Python original also passes the new
  version, or if the change is a deliberate behavior change recorded in `PLAN.md`.
- **Never commit real assets.** `fixtures/golden/` holds thumbnails from the user's purchased
  token packs and is gitignored. CI and committed tests use synthetic images only.
- **Keep the on-disk format compatible** so an existing library imports as-is: SQLite schema
  from `../asset-library/app/db.py`, thumbnails at `thumbs/<hash[:2]>/<hash>.webp`, SHA-1
  content hashes, embeddings as little-endian f32 L2-normalized blobs. Schema changes go
  through `PRAGMA user_version` migrations.

## Launch contract (both implementations)

```
<server> --no-browser --port N      # serves http://127.0.0.1:N, API under /api
ASSET_LIBRARY_DATA=<dir>            # library.db + thumbs/ live here
ASSET_LIBRARY_MODELS=<dir>          # CLIP model cache
ASSET_LIBRARY_AI=0|1                # 0 = colours only
```

## Commands

```bash
parity/.venv/bin/pytest parity                         # against the Python original
PARITY_IMPL=rust parity/.venv/bin/pytest parity        # against target/debug/asset-library
parity/.venv/bin/python tools/make_golden.py           # regenerate fixtures/golden from ../asset-library
```
