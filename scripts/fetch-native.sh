#!/usr/bin/env bash
# macOS counterpart of fetch-native.ps1: downloads the prebuilt native
# libraries chatty links against or loads at runtime. Nothing is compiled.
#
#   native/transcribe/        install prefix for transcribe-cpp-sys (TRANSCRIBE_DIR)
#     lib/*.dylib             libtranscribe + its ggml (Metal + CPU)
#     lib/transcribe-link.json  link manifest the sys crate's build.rs reads
#   native/llama/             llama.cpp dylibs for the cleanup pass (Metal)
#
# Apple Silicon only: both projects ship arm64 macOS builds. The
# transcribe.cpp version is read from Cargo.toml, as on Windows.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
native=$root/native
force=${1:-}

tc_version=$(sed -nE 's/^transcribe-cpp *= *\{ *version *= *"=?([0-9.]+)".*/\1/p' Cargo.toml)
[ -n "$tc_version" ] || { echo "transcribe-cpp version not found in Cargo.toml" >&2; exit 1; }
case "$tc_version" in
  0.5.1) tc_sha=a8022e3573ce3fb0dc22731c828c9a2620b3cab5fb98c34fb8b995e6ad202daa ;;
  *) echo "no sha256 pinned for transcribe.cpp $tc_version" >&2; exit 1 ;;
esac
llama_build=b12774
llama_sha=70311c4751ed3221e63f47769327dd088d7400f76dcfb81c66ec843db165814d

dl=$native/.download
mkdir -p "$dl"

get_verified() { # url out sha
  if [ ! -f "$2" ]; then
    echo "download $1"
    curl -fsSL --retry 3 -o "$2" "$1"
  fi
  actual=$(shasum -a 256 "$2" | cut -d' ' -f1)
  if [ "$actual" != "$3" ]; then rm -f "$2"; echo "sha256 mismatch for $2 ($actual)" >&2; exit 1; fi
}

# --- transcribe.cpp ---------------------------------------------------------
tc_dir=$native/transcribe
if [ "$force" = "--force" ] || [ "$(cat "$tc_dir/.version" 2>/dev/null)" != "$tc_version-macos" ]; then
  asset=transcribe-native-$tc_version-macos-arm64-metal.tar.gz
  get_verified "https://github.com/handy-computer/transcribe.cpp/releases/download/v$tc_version/$asset" "$dl/$asset" "$tc_sha"
  rm -rf "$tc_dir" "$dl/tc-extract"
  mkdir -p "$dl/tc-extract" "$tc_dir/lib" "$tc_dir/include"
  tar -xzf "$dl/$asset" -C "$dl/tc-extract"
  bundle=$(find "$dl/tc-extract" -mindepth 1 -maxdepth 1 -type d | head -1)
  cp "$bundle"/*.dylib "$tc_dir/lib/"
  cp -R "$bundle/licenses" "$tc_dir/licenses"
  cp "$bundle/contract.json" "$tc_dir/"
  # The manifest transcribe-cpp-sys reads on the TRANSCRIBE_DIR path. The
  # dylibs reference each other through @loader_path; chatty's build.rs adds
  # the rpaths that find libtranscribe itself.
  cat > "$tc_dir/lib/transcribe-link.json" <<JSON
{
  "version": "$tc_version",
  "commit": "",
  "shared": true,
  "backend_dl": false,
  "backends": ["metal", "cpu"],
  "metal_embed": true,
  "include_dir": "include",
  "lib_dir": "lib",
  "libraries": ["transcribe"],
  "library_paths": [],
  "system_libs": [],
  "frameworks": [],
  "link_flags": []
}
JSON
  echo "$tc_version-macos" > "$tc_dir/.version"
  echo "transcribe.cpp $tc_version -> $tc_dir"
fi

# --- llama.cpp --------------------------------------------------------------
# Its ggml dylibs are versioned (libggml.1.dylib) and resolve through the
# library's own @loader_path rpath, so they never collide with
# transcribe.cpp's libggml.dylib in the same process.
ll_dir=$native/llama
if [ "$force" = "--force" ] || [ "$(cat "$ll_dir/.version" 2>/dev/null)" != "$llama_build-macos" ]; then
  asset=llama-$llama_build-bin-macos-arm64.tar.gz
  get_verified "https://github.com/ggml-org/llama.cpp/releases/download/$llama_build/$asset" "$dl/$asset" "$llama_sha"
  rm -rf "$ll_dir" "$dl/ll-extract"
  mkdir -p "$dl/ll-extract" "$ll_dir"
  tar -xzf "$dl/$asset" -C "$dl/ll-extract"
  src=$dl/ll-extract/llama-$llama_build
  # libllama and the versioned ggml libraries it loads, symlinks kept. (A
  # duplicate-class warning about GGMLMetalClass from the two Metal backends
  # is harmless: both embed their shader library.)
  for f in "$src"/libllama.1*.dylib "$src"/libggml*.1*.dylib; do
    cp -P "$f" "$ll_dir/"
  done
  cp "$src/LICENSE" "$ll_dir/LICENSE"
  echo "$llama_build-macos" > "$ll_dir/.version"
  echo "llama.cpp $llama_build -> $ll_dir"
fi
