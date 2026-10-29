"""Build fixtures/golden/ from the real library in ../asset-library (read-only).

The original's DB is copied first, and its own analysis code is imported against the copy,
so expected values come from the Python implementation itself. Output (gitignored, since
the thumbnails are the user's purchased art):

  library.db        copy of the full library (suggestions depend on every asset)
  thumbs/<h>.webp   thumbnails for the sampled assets
  features.json     per sampled asset: id, hash, rel_path, tags, stored colours
  embeddings.f32    stored image embeddings for the sample, row-major little-endian f32
  text.json         text queries: embedding + top-20 asset ids over the whole library
  suggestions.json  tag suggestions for the sampled assets

Usage: python tools/make_golden.py [--sample 300]
"""

import argparse
import json
import os
import shutil
import sqlite3
import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
ORIGINAL = Path(os.environ.get("ASSET_LIBRARY_ORIGINAL", ROOT.parent / "asset-library"))
OUT = ROOT / "fixtures" / "golden"
MODELS = ROOT / "models"

QUERIES = ["a red dragon", "skeleton warrior with a sword", "a wizard in blue robes",
           "wolf", "humanoid with an octopus head and face tentacles", "a treasure chest",
           "a solid blue square", "elf archer"]


def copy_db(dest: Path) -> None:
    for suffix in ("", "-wal", "-shm"):
        src = ORIGINAL / "data" / f"library.db{suffix}"
        if src.exists():
            shutil.copy2(src, dest.parent / f"{dest.name}{suffix}")
    conn = sqlite3.connect(dest)  # fold the WAL into the copy
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.execute("PRAGMA journal_mode = DELETE")
    conn.close()


def import_original(data_dir: Path):
    """Import the original's modules against our copy without writing into its tree."""
    sys.dont_write_bytecode = True
    os.environ["ASSET_LIBRARY_DATA"] = str(data_dir)
    os.environ["ASSET_LIBRARY_MODELS"] = str(MODELS)
    sys.path.insert(0, str(ORIGINAL))
    from app import analysis, db
    return analysis, db


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--sample", type=int, default=300)
    args = p.parse_args()

    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "thumbs").mkdir(parents=True)
    db_path = OUT / "library.db"
    copy_db(db_path)

    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """SELECT a.id, a.file_hash, a.rel_path, f.colors, f.embedding FROM assets a
           JOIN image_features f ON f.file_hash = a.file_hash
           WHERE a.missing = 0 AND f.embedding IS NOT NULL
           GROUP BY a.file_hash ORDER BY a.file_hash""").fetchall()
    step = max(1, len(rows) // args.sample)
    sample = rows[::step][:args.sample]

    features, vectors = [], []
    for r in sample:
        thumb = ORIGINAL / "data" / "thumbs" / r["file_hash"][:2] / f"{r['file_hash']}.webp"
        if not thumb.exists():
            continue
        shutil.copy2(thumb, OUT / "thumbs" / thumb.name)
        tags = [t[0] for t in conn.execute(
            "SELECT t.name FROM asset_tags x JOIN tags t ON t.id = x.tag_id "
            "WHERE x.asset_id = ? ORDER BY t.name", (r["id"],))]
        features.append({"id": r["id"], "hash": r["file_hash"], "rel_path": r["rel_path"],
                         "tags": tags, "colors": json.loads(r["colors"])})
        vectors.append(np.frombuffer(r["embedding"], dtype="<f4"))
    conn.close()
    (OUT / "features.json").write_text(json.dumps(features, indent=1))
    np.stack(vectors).astype("<f4").tofile(OUT / "embeddings.f32")
    print(f"sampled {len(features)} of {len(rows)} assets")

    analysis, dbmod = import_original(OUT)
    db = dbmod.Database(db_path)
    sugg = analysis.suggest(db, [f["id"] for f in features])
    (OUT / "suggestions.json").write_text(json.dumps({str(k): v for k, v in sugg.items()}, indent=1))
    print(f"suggestions for {len(sugg)} assets")

    if analysis.AI_AVAILABLE and any(MODELS.glob("models--*")):
        text = [{"query": q, "embedding": analysis.embed_text(q).tolist(),
                 "top": [aid for aid, _ in analysis.rank_text(db, q, limit=20)]} for q in QUERIES]
        (OUT / "text.json").write_text(json.dumps(text))
        print(f"text queries: {len(text)}")
    else:
        print("skipped text queries (fastembed or models/ missing)")


if __name__ == "__main__":
    main()
