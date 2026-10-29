# Port plan

Goal: a native, installable Asset Library for Windows, macOS and Linux that behaves like the
Python original and can open an existing library.

Stack: Tauri v2 shell; axum serving the existing REST API in-process (so `static/` UI and the
parity suite carry over); rusqlite (bundled, FTS5); `image` + `webp` + `fast_image_resize`;
rayon; fastembed-rs / ort for CLIP.

Development happens in WSL; installers are built by GitHub Actions.

## Next up (resume here)

M0 is done: all 17 parity tests pass against the Python original with no harness changes
(`test_color_profile` needed no tolerance). `fixtures/golden/` holds 300 sampled assets,
8 text queries and suggestions. System packages and `parity/.venv` are installed.

1. Start M1: Cargo workspace, schema, scanner.

## Milestones

- [x] **M0 Harness.** Black-box parity suite (`parity/`) passing against the Python original;
      golden fixtures from the real library (`tools/make_golden.py`).
- [ ] **M1 Storage and scanning.** Identical schema, SHA-1 hashing, move/missing detection,
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

## Decisions log

- 2026-10-28: Full Rust rewrite chosen over a Python sidecar. Develop in WSL, CI for installers,
  target Windows + macOS + Linux. The benchmark moved from the Python app to the Rust port (M5)
  so friends can run it without Python.
