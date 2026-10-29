# Port plan

Goal: a native, installable Asset Library for Windows, macOS and Linux that behaves like the
Python original and can open an existing library.

Stack: Tauri v2 shell; axum serving the existing REST API in-process (so `static/` UI and the
parity suite carry over); rusqlite (bundled, FTS5); `image` + `webp` + `fast_image_resize`;
rayon; fastembed-rs / ort for CLIP.

Development happens in WSL; installers are built by GitHub Actions.

## Next up (resume here)

M1 is done. `crates/asset-library` has the schema (`user_version` 1), scanner, sources,
scan, asset list/get/patch and tag list endpoints. `PARITY_IMPL=rust` passes the 8 parity
tests within M1's scope. The 9 that fail need M2 (thumbs, export), M3 (bulk, tag rename),
M4 (colours, suggestions) or M5 (CLIP). The search query language (`search.rs`) was ported
early, because the move/missing tests can only check their results through it.

A rescan of a copy of the real library (6785 files, source path rewritten to `/mnt/c/...`)
updated 0 files, so Rust's `mtime` matches what Python stored and imports aren't rehashed.

1. Start M2: thumbnails at `thumbs/<hash[:2]>/<hash>.webp` (port `app/thumbs.py`),
   the background pipeline (`jobs.py`), `/api/assets/{id}/thumb` and `/file`, zip and
   folder export (`app/export.py`). Hook the pipeline into `AppState::start_scan` and
   startup (the `TODO(M2)`s); `jobs::start` must set the warmer's `running` flag before it
   returns (see `Scanner::scan_in_background`). Also replace the `thumbs` stub in
   `scan_status`.

## Milestones

- [x] **M0 Harness.** Black-box parity suite (`parity/`) passing against the Python original;
      golden fixtures from the real library (`tools/make_golden.py`).
- [x] **M1 Storage and scanning.** Identical schema, SHA-1 hashing, move/missing detection,
      folder tags, junk skipping, sources and scan-status API.
- [ ] **M2 Thumbnails and files.** Lossy WebP thumbs, parallel warmer, file serving,
      zip and copy-to-folder export.
- [ ] **M3 Search.** Query language to SQL, FTS5, tags API (rename/merge), bulk edits.
- [ ] **M4 Colours.** Port `color_profile`; compare against golden colours.
- [ ] **M5 CLIP.** Image/text embeddings, similarity index, `similar:` / `~"..."`, tag
      suggestions; compare against golden embeddings and suggestions. Add an
      `asset-library bench` subcommand and send an early build to the friend with the weakest
      machine (images/sec, peak RAM) to decide on int8 and low-end defaults.
- [ ] **M6 Full-library diff.** Serve the existing `static/` UI; run both servers on copies of
      the real DB and diff query results (`tools/diff_servers.py`).
- [ ] **M7 Desktop app.** Tauri window, per-user data dir, migrations, native folder picker,
      native drag-out, tray, single instance, settings (AI opt-in with model download, CPU
      thread cap, below-normal priority, pause on battery, batch size), import of an existing
      Python library.
- [ ] **M8 Release.** tauri-action matrix (Win/macOS/Linux), updater signing key, GitHub
      Releases, in-app "new version" prompt.

## Known, accepted differences

- Colour fractions and CLIP vectors differ slightly from Pillow/fastembed-Python output. On
  first run the Rust app recomputes colours and embeddings for imported libraries.
- Scan status stays `running` until all requested sources are scanned and the follow-up job
  has started. The original briefly reports idle between sources and before thumbnailing.
- File and folder names that aren't valid UTF-8 are skipped. The original aborted the
  whole scan on them.
- `/api/meta` reports `ai: false` until M5.
- Stored `extra` JSON is compact (`{"a":1}`), not Python's `{"a": 1}`. Both parse the same.

## Decisions log

- 2026-10-28: Full Rust rewrite chosen over a Python sidecar. Develop in WSL, CI for installers,
  target Windows + macOS + Linux. The benchmark moved from the Python app to the Rust port (M5)
  so friends can run it without Python.
