# Port plan

Goal: a native, installable Asset Library for Windows, macOS and Linux that behaves like the
Python original and can open an existing library.

Stack: Tauri v2 shell; axum serving the existing REST API in-process (so `static/` UI and the
parity suite carry over); rusqlite (bundled, FTS5); `image` + `webp` + `fast_image_resize`;
rayon; fastembed-rs / ort for CLIP.

Development happens in WSL; installers are built by GitHub Actions.

## Next up (resume here)

M9 (Dungeondraft packs, designed below) is built; M8 (properties) is built. Both wait for
the user to try them in the Windows installer (`tools/build_windows.sh`). Then M10 (release).

M9 checks so far: 7 regression tests (`parity/test_packs.py`, synthetic packs), unit tests in
`ddpack.rs` and `location.rs`, all 61 real packs parse (`DDPACK_DIR=... cargo test real_packs
-- --ignored`), three real packs (8,103 images) scan in 4.5 s with thumbnails and colours 13 s
later and a 0.2 s unchanged rescan, and export-every-result driven in headless Chromium.
All 61 real packs (release build, WSL reading /mnt/c): 92,428 images scanned in 84 s with no
errors, 91,405 unique thumbnails + colours in 200 s with 0 failures (864 MB of thumbnails,
87 MB database), searches 6-160 ms, facets + tags 271 ms, unchanged rescan 0.5 s. The first
full run found reindex_fts exceeding SQLite's variable limit (fixed: id lists are chunked).

M8 checks: 25 regression tests (`parity/`), unit tests in `properties.rs` and `search.rs`, a
headless-Chromium run through the UI, and suggestions on the real library.

Setup for a fresh checkout: `python tools/fetch_onnxruntime.py` (ONNX Runtime 1.30.0 into
`vendor/onnxruntime/<platform>`; debug builds find it there, and the desktop bundle ships it as a
resource).

### Testing M7

```bash
python tools/fetch_onnxruntime.py                        # once
cargo run -p asset-library-desktop                       # per-user data dir
ASSET_LIBRARY_DATA=/tmp/al-desktop ASSET_LIBRARY_MODELS=$PWD/models \
  cargo run -p asset-library-desktop                     # scratch library, reuse models/
```

- Data lives in `~/.local/share/io.github.patch85.asset-library` (Linux),
  `%APPDATA%\io.github.patch85.asset-library` (Windows), `~/Library/Application Support/...`
  (macOS). A library in the old `com.assetlibrary.desktop` folder is moved there on first launch.
- WSL limitations: no tray (no session D-Bus, so closing the window quits), and drags can't
  cross from WSLg windows into Windows Explorer. On WSL the app forces software GL; without
  it WebKitGTK loads nothing. Tray, drag-out to Explorer and importing the real (Windows-path)
  library need a native Windows build: rustup + VS Build Tools, then the same two commands.
- `ASSET_LIBRARY_LOG=1` logs every request to stderr.

Checked so far (WSL): the window loads the UI and API, scanning/thumbnails/analysis run,
settings persist and turning AI on starts embedding, and importing a copy of the real library
brings in all 6785 assets without changing the source DB.

### Waiting on the user

- Bench for the friend is on hold (2026-10-28): the user first tests a local Windows
  installer. Int8 and low-end defaults wait on that bench.
- Icons in `crates/desktop/icons` are generated placeholders; the user is fine with them.

### Local Windows installer

`tools/build_windows.sh` cross-builds an unsigned NSIS installer from WSL into
`target/x86_64-pc-windows-msvc/release/bundle/nsis/` (`tools/build_windows.sh check` only
type-checks). Needs `sudo apt install -y nsis lld llvm clang`, `rustup target add
x86_64-pc-windows-msvc`, `cargo install tauri-cli cargo-xwin`. Workarounds in the script:
`-msse4.1` for libwebp under clang-cl, and an NSIS overlay adding `Win/RestartManager.nsh`
(missing from Ubuntu 22.04's NSIS 3.08) through a `makensis` wrapper, because tauri-bundler
clears `NSISDIR`. First build: 2026-10-28, 11.3 MiB, awaiting the user's test on Windows.

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

## M8 design: properties

Tags stay an open vocabulary for what the subject is. Properties are per-type fields with
one value each, for how the asset is made or used (the real library hides creature size in
2700 filenames, and pack names like `fa_tokens` live in tags).

- Schema (migration 2): `fields(id, asset_type, key, label, kind, options, position)` with
  kind `choice` (options = JSON list), `number` or `text`; `asset_fields(asset_id, field_id,
  value)`, numbers stored as REAL so comparisons work. Seeded defaults: token view
  (top-down/portrait/isometric/side), size (tiny…gargantuan), pack (text); map grid
  (number, px per square), variant (day/night/dawn/dusk/rain/snow/fog), pack (text).
- Editable: add/rename/reorder/delete fields; add/rename/remove choice options (renames
  update values, removals clear them). Keys can't be reserved query words.
- Values: asset GET/PATCH `fields: {key: value|null}`; bulk `set_fields`. Changing an
  asset's type moves values to same-key fields of the new type and drops the rest.
- Search: `size:large`, `-size:large`, `pack:"forgotten adventures"`, `grid>=70`,
  `has:size` / `-has:size`. Sidebar sections with value counts (`/api/facets`).
- Suggestions alongside tag suggestions: filename words matching a choice option; majority
  value among assets with the same source and pixel size; look-alike votes (CLIP
  neighbours); majority among files in the same folder (pack).
- "Move tag to property": set a field value on every asset with the tag, then drop the tag.

## M9 design: Dungeondraft packs

The user's 61 `.dungeondraft_pack` files hold ~92k images (7.3 GB): objects 86.8k, terrain
1.5k, paths 1.4k, patterns 1.35k, portals 592, walls 355, tilesets 180, roofs 120, lights 72,
materials 10. Format: Godot 3 PCK v1 (`GDPC`, file table of path/offset/size/md5) with
`res://packs/<id>/pack.json` (name, id, version, author), `data/default.dungeondraft_tags`
(`{"tags": {name: [paths]}, "sets": ...}`; 6 of 42 have trailing commas) and images stored
as plain PNG/WebP under `textures/<kind>/...`.

Decisions (user, 2026-10-29): index in place (no extraction to disk); Dungeondraft tags and
in-pack folders both become tags; packs get the same background analysis as everything else.

- Schema (migration 3): `assets.container` (the pack's path within the source),
  `entry_offset`, `entry_size`; `containers(source_id, rel_path, size, mtime, name, author,
  version)`. Entry `rel_path` = `<pack rel path>/<path inside packs/<id>/>`, so it's unique,
  searchable and moves by hash when a pack is updated to a new file name.
- Reading goes through one abstraction (plain file or pack entry by offset): thumbnails,
  `/file`, export, zip. Drag and open first extract to `data/extracted/` (pruned after a week);
  reveal shows the pack file.
- Scanning: a pack is re-read only when its size or mtime changes; otherwise its known entries
  count as seen. Type by folder: objects -> prop, terrain/patterns/materials -> texture,
  tilesets -> tile, walls/paths/portals/roofs/lights -> other. New entries get Pack (pack.json
  name) and Kind (the folder) properties; migration 3 adds `pack` to prop/tile/texture/other and
  `kind` choices to texture/other. Tags: Dungeondraft tags (leading `.`/`[XX] ` dropped,
  `Colorable`/`Testing` skipped) always; in-pack folders when the source has folder tags on.
- Export (copy/zip) writes entry bytes out under `<pack name>/<inner path>`, and can export
  every result of the current search, not just loaded/selected cards.

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
- [ ] **M8 Properties.** Editable per-type fields with search, facets, bulk edit, suggestions
      and tag-to-property moves (design above).
- [ ] **M9 Dungeondraft packs.** Index `.dungeondraft_pack` contents in place (design above).
- [ ] **M10 Release.** tauri-action matrix (Win/macOS/Linux), updater signing key, GitHub
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
- 2026-10-28: Parity with the Python app is no longer a goal; it was reached at M6. `parity/`
  becomes the Rust app's regression suite, the schema may diverge through migrations, and
  importing Python libraries must keep working. `tools/diff_servers.py` and the golden
  comparisons stay as records but may start to differ by design.
- 2026-10-28: Properties (M8) come before the release, with user-editable field definitions.
- 2026-10-29: Dungeondraft packs (M9) are indexed in place before the release; their tags and
  in-pack folders become tags.
- 2026-10-29: App identifier `io.github.patch85.asset-library` (repo github.com/Patch85/
  asset-library-rs). It names the per-user data dir, so it must not change after release.
  Test installs' `com.assetlibrary.desktop` data dir is moved to the new one on first launch.
