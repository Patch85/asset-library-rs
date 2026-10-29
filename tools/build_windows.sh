#!/usr/bin/env bash
# Cross-build the Windows NSIS installer from Linux/WSL.
#
# Needs: sudo apt install nsis lld llvm clang; rustup target add x86_64-pc-windows-msvc;
# cargo install tauri-cli cargo-xwin. cargo-xwin downloads Microsoft's CRT/SDK headers
# (accepting Microsoft's license for them).
#
# Usage: tools/build_windows.sh [check]   # "check" only type-checks the workspace
set -euo pipefail
cd "$(dirname "$0")/.."

# clang-cl (unlike MSVC's cl) won't compile libwebp's SSE4.1 files without the target
# feature; libwebp still picks SIMD paths at runtime. cargo-xwin folds plain CFLAGS into its
# per-target flags (setting CFLAGS_<target> ourselves gets overwritten).
export CFLAGS="${CFLAGS:-} -msse4.1"

if [[ "${1:-}" == "check" ]]; then
    exec cargo xwin check --target x86_64-pc-windows-msvc --workspace
fi

[[ -f vendor/onnxruntime/win-x64/onnxruntime.dll ]] ||
    python3 tools/fetch_onnxruntime.py --platform win-x64

# Ubuntu 22.04's NSIS 3.08 lacks Win/RestartManager.nsh, which Tauri's installer script
# includes. Overlay a copy of the system NSIS data with that header from upstream (plain
# defines).
if [[ ! -f /usr/share/nsis/Include/Win/RestartManager.nsh ]]; then
    nsis=vendor/nsis
    if [[ ! -f $nsis/Include/Win/RestartManager.nsh ]]; then
        rm -rf "$nsis" && cp -r /usr/share/nsis "$nsis"
        curl -sfL -o "$nsis/Include/Win/RestartManager.nsh" \
            https://raw.githubusercontent.com/kichik/nsis/v311/Include/Win/RestartManager.nsh
    fi
    # tauri-bundler clears NSISDIR before running makensis, so wrap makensis on PATH.
    mkdir -p "$nsis/wrapper"
    printf '#!/bin/sh\nNSISDIR=%q exec /usr/bin/makensis "$@"\n' "$PWD/$nsis" > "$nsis/wrapper/makensis"
    chmod +x "$nsis/wrapper/makensis"
    export PATH="$PWD/$nsis/wrapper:$PATH"
fi
cd crates/desktop
cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis
ls -l ../../target/x86_64-pc-windows-msvc/release/bundle/nsis/
