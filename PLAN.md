# Port plan

Goal: a native, installable Asset Library for Windows, macOS and Linux that behaves like the
Python original and can open an existing library.

Stack: Tauri v2 shell; axum serving the existing REST API in-process (so `static/` UI and the
parity suite carry over); rusqlite (bundled, FTS5); `image` + `webp` + `fast_image_resize`;
rayon; fastembed-rs / ort for CLIP.

Development happens in WSL; installers are built by GitHub Actions.

## Next up (resume here)

M7 is built and waiting for the user to test it (see "Testing M7" below); then start M8.
M0–M6 are done: all 17 parity tests pass with `PARITY_IMPL=rust`, `tools/diff_servers.py`
finds 245/245 deterministic responses identical on the real library, and `cargo test` runs
the golden comparisons when `fixtures/golden/` and `models/` are present.

Setup for a fresh checkout: `python tools/fetch_onnxruntime.py` (ONNX Runtime 1.30.0 into
`vendor/onnxruntime/lib`; debug builds find it there, and the desktop bundle ships it as a
resource).

### Testing M7

```bash
python tools/fetch_onnxruntime.py                        # once
cargo run -p asset-library-desktop                       # per-user data dir
ASSET_LIBRARY_DATA=/tmp/al-desktop ASSET_LIBRARY_MODELS=$PWD/models \
  cargo run -p asset-library-desktop                     # scratch library, reuse models/
```

- Data lives in `~/.local/share/com.assetlibrary.desktop` (Linux),
  `%APPDATA%\com.assetlibrary.desktop` (Windows), `~/Library/Application Support/...` (macOS).
- WSL limitations: no tray (no session D-Bus, so closing the window quits), and drags can't
  cross from WSLg windows into Windows Explorer. On WSL the app forces software GL; without
  it WebKitGTK loads nothing. Tray, drag-out to Explorer and importing the real (Windows-path)
  library need a native Windows build: rustup + VS Build Tools, then the same two commands.
- `ASSET_LIBRARY_LOG=1` logs every request to stderr.

Checked so far (WSL): the window loads the UI and API, scanning/thumbnails/analysis run,
settings persist and turning AI on starts embedding, and importing a copy of the real library
brings in all 6785 assets without changing the source DB.

### Waiting on the user

- Send a `bench` build to the friend with the weakest machine, then decide on int8 and
  low-end defaults (batch size, threads). Dev machine numbers are under M5 results below.
- Confirm the app identifier `com.assetlibrary.desktop` before the first release: it names
  the per-user data dir, so changing it later strands existing libraries.
- The icons in `crates/desktop/icons` are generated placeholders.

### Results so far

- Rescan of the real library updated 0 of 6785 files (Rust `mtime` matches Python's).
- Thumbnails: 6785 in 44 s (154/s, 21 threads) with 0 failures; 300 golden thumbnails match
  in size, pixels differ by 1.1/255 on average (3.3 max).
- Colours: mean L1 0.006 against Python's; dominant colour agrees for 298/300.
- CLIP: image embeddings cosine 0.9997 mean (0.994 min); text cosine 1.0000 with identical
  top-20s; suggestions identical for all 300 sampled assets; image search identical in the
  full-library diff.
- `bench` (22 CPUs, WSL, 300 real tokens): thumbnails 299/s, colours 705/s, CLIP 50/s at
  batch 64 with 21 threads, 24/s with 2 threads; peak RSS 0.7–1.4 GB (includes the bench
  holding every decoded image).

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
- [ ] **M7 Desktop app.** (built; awaiting user testing) Tauri window, per-user data dir, migrations, native folder picker,
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
- API extensions the original doesn't have: `/api/meta` `desktop`; `/api/scan/status`
  `paused` and `analysis.message`; `/api/settings`, `/api/import`, `/api/drag`,
  `/api/assets/{id}/open`, `/api/export/zip-save`. The plain server's `/api/pick-folder`
  returns an empty path (the original opened a tkinter dialog).
- The UI asks for confirmations and tag renames with an in-page dialog instead of
  `prompt()`/`confirm()`, which not every webview shows.
- The desktop app starts with AI off (opt-in in Settings, which downloads the model); the
  plain server keeps the original's default (on unless `ASSET_LIBRARY_AI=0`).
- Stored `extra` JSON is compact (`{"a":1}`), not Python's `{"a": 1}`. Both parse the same.

## Decisions log

- 2026-10-28: Full Rust rewrite chosen over a Python sidecar. Develop in WSL, CI for installers,
  target Windows + macOS + Linux. The benchmark moved from the Python app to the Rust port (M5)
  so friends can run it without Python.
- 2026-10-28: ONNX Runtime is loaded dynamically (ort `load-dynamic`), pinned to 1.30.0.
  ort's prebuilt static library needs glibc 2.38+; Microsoft's build needs 2.28.
- 2026-10-28: Imports keep the Python thumbnails, recompute colours, and re-embed only when AI
  is on, keeping the old vectors (tagged `embed_model ... (python)`) usable meanwhile.
- 2026-10-28: The desktop app hosts the HTTP server in-process and reaches native features
  through HTTP endpoints (`Desktop` trait), so the page needs no Tauri IPC or capabilities.
