# Port plan

Goal: a native, installable Asset Library for Windows, macOS and Linux that behaves like the
Python original and can open an existing library.

Stack: Tauri v2 shell; axum serving the existing REST API in-process (so `static/` UI and the
parity suite carry over); rusqlite (bundled, FTS5); `image` + `webp` + `fast_image_resize`;
rayon; fastembed-rs / ort for CLIP.

Development happens in WSL; installers are built by GitHub Actions.

## Next up (resume here)

M1–M5 are done. `PARITY_IMPL=rust` passes all 17 parity tests, including the real-model
text search. `cargo test` also runs golden comparisons when `fixtures/golden/` and `models/`
are present.

Setup for a fresh checkout: `python tools/fetch_onnxruntime.py` (ONNX Runtime 1.30.0 into
`vendor/onnxruntime/lib`; debug builds find it there, release builds look next to the exe).

Real-library checks (copy of the golden DB, source path rewritten to `/mnt/c/...`):
- A rescan updated 0 of 6785 files, so Rust's `mtime` matches what Python stored.
- The release build warmed all 6785 thumbnails in 44 s (154/s, 21 threads) with 0 failures.
  The 300 golden thumbnails match in size; pixels differ by 1.1/255 on average (3.3 max).
- Colours: mean L1 0.006 against Python's, dominant colour agrees for 298/300.
- Image embeddings: cosine 0.9997 mean, 0.994 min. Text embeddings: cosine 1.0000, top-20
  identical for all 8 queries. Suggestions: identical for all 300 sampled assets.
- `bench` on the dev machine (22 CPUs, WSL, 300 real tokens): thumbnails 299/s, colours
  705/s, CLIP 50/s at batch 64 with 21 threads, 24/s with 2 threads. Peak RSS 1.0–1.4 GB
  (includes the bench holding every decoded test image).

Waiting on the user: send a `bench` build to the friend with the weakest machine, then
decide on int8 and low-end defaults (batch size, threads).

M6: the UI (copied from the original into `crates/asset-library/static`) is embedded in
the binary. `tools/diff_servers.py` on the real library: 245/245 deterministic responses
identical, and image search identical (20/20 top-20, 100/100 top-100) on every query.

1. Start M7: `crates/desktop` (Tauri v2) hosting the same server on a random localhost port.

## Milestones

- [x] **M0 Harness.** Black-box parity suite (`parity/`) passing against the Python original;
      golden fixtures from the real library (`tools/make_golden.py`).
- [x] **M1 Storage and scanning.** Identical schema, SHA-1 hashing, move/missing detection,
      folder tags, junk skipping, sources and scan-status API.
- [x] **M2 Thumbnails and files.** Lossy WebP thumbs, parallel warmer, file serving,
      zip and copy-to-folder export.
- [x] **M3 Search.** Query language to SQL, FTS5, tags API (rename/merge), bulk edits.
- [x] **M4 Colours.** Port `color_profile`; compare against golden colours.
- [x] **M5 CLIP.** Image/text embeddings, similarity index, `similar:` / `~"..."`, tag
      suggestions; compare against golden embeddings and suggestions. Add an
      `asset-library bench` subcommand and send an early build to the friend with the weakest
      machine (images/sec, peak RAM) to decide on int8 and low-end defaults.
- [x] **M6 Full-library diff.** Serve the existing `static/` UI; run both servers on copies of
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
- ONNX Runtime is loaded at runtime from Microsoft's official build, which no longer
  ships for Intel Macs. AI features there need a self-supplied `ORT_DYLIB_PATH`.
- Suggestions break score ties by tag name; the original's order for ties depended on
  Python's per-process string hashing.
- Stored `extra` JSON is compact (`{"a":1}`), not Python's `{"a": 1}`. Both parse the same.

## Decisions log

- 2026-10-28: Full Rust rewrite chosen over a Python sidecar. Develop in WSL, CI for installers,
  target Windows + macOS + Linux. The benchmark moved from the Python app to the Rust port (M5)
  so friends can run it without Python.
- 2026-10-28: ONNX Runtime is loaded dynamically (ort `load-dynamic`), pinned to 1.30.0.
  ort's prebuilt static library needs glibc 2.38+; Microsoft's build needs 2.28.
