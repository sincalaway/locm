#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${CARGO:-cargo}"

case "$(uname -s)" in
  Linux*) ;;
  *)
    echo "This cross-build script requires a Linux host. Use the native Windows MSVC workflow on Windows." >&2
    exit 2
    ;;
esac

linker="${LOCM_WINDOWS_GNU_LINKER:-${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-${CC_x86_64_pc_windows_gnu:-x86_64-w64-mingw32-gcc}}}"
if [[ "$linker" == "$repo_root/scripts/windows-gnu-linker.sh" ]]; then
  echo "LOCM_WINDOWS_GNU_LINKER must point to MinGW-w64 GCC, not this wrapper." >&2
  exit 2
fi
if [[ "$linker" == */* ]]; then
  if [[ ! -x "$linker" ]]; then
    echo "Windows GNU linker is not executable: $linker" >&2
    exit 2
  fi
elif ! command -v "$linker" >/dev/null 2>&1; then
  echo "Windows GNU linker was not found on PATH: $linker" >&2
  echo "Install MinGW-w64 or set LOCM_WINDOWS_GNU_LINKER to its GCC driver." >&2
  exit 2
fi

export LOCM_WINDOWS_GNU_LINKER="$linker"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$repo_root/scripts/windows-gnu-linker.sh"
cd "$repo_root"
exec "$cargo_bin" build --release --locked --features desktop --bin local-engine-manager --target x86_64-pc-windows-gnu "$@"
