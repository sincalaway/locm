#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

case "$(uname -s)" in
  Linux*) ;;
  *)
    echo "This script is for Linux hosts; use the native Windows Tauri build on Windows." >&2
    exit 2
    ;;
esac

missing=()
for tool in cargo makensis llvm-rc lld-link cargo-xwin; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    missing+=("$tool")
  fi
done
if ! "$repo_root/scripts/clang-cl" --version >/dev/null 2>&1; then
  missing+=("clang-cl or clang-18 (MSVC C compiler for native dependencies)")
fi
if ! cargo tauri --version >/dev/null 2>&1; then
  missing+=("cargo tauri (install tauri-cli)")
fi
if ((${#missing[@]})); then
  printf 'Missing NSIS cross-build tools: %s\n' "${missing[*]}" >&2
  echo "See README.md, 'Linux 上交叉构建 Windows x64 NSIS 安装器', for setup instructions." >&2
  exit 2
fi

if ! rustup target list --installed | grep -qx 'x86_64-pc-windows-msvc'; then
  echo "Rust target x86_64-pc-windows-msvc is not installed; run: rustup target add x86_64-pc-windows-msvc" >&2
  exit 2
fi

compiler_shim_dir="$(mktemp -d "${TMPDIR:-/tmp}/locm-clang-cl.XXXXXX")"
trap 'rm -rf "$compiler_shim_dir"' EXIT
ln -s "$repo_root/scripts/clang-cl" "$compiler_shim_dir/clang-cl"
export PATH="$compiler_shim_dir:$PATH"
export CC_x86_64_pc_windows_msvc="$compiler_shim_dir/clang-cl"
# tauri-build's static_vcruntime selects static startup/VCRuntime plus dynamic UCRT.
# Do not additionally force +crt-static; this cross-link setup has unresolved CRT symbols.
cargo tauri build \
  --features desktop \
  --runner cargo-xwin \
  --target x86_64-pc-windows-msvc \
  --bundles nsis \
  -- \
  --no-default-features
