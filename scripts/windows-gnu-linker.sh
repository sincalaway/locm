#!/usr/bin/env bash
set -euo pipefail

linker="${LOCM_WINDOWS_GNU_LINKER:-}"
if [[ -z "$linker" ]]; then
  echo "LOCM_WINDOWS_GNU_LINKER must name the MinGW-w64 GCC linker driver." >&2
  exit 2
fi
if [[ "$linker" == */* ]]; then
  if [[ ! -x "$linker" ]]; then
    echo "Windows GNU linker is not executable: $linker" >&2
    exit 2
  fi
elif ! command -v "$linker" >/dev/null 2>&1; then
  echo "Windows GNU linker was not found on PATH: $linker" >&2
  exit 2
fi

# Rust's raw-dylib linkage for kernel32.dll can reach GNU ld as -lKernel32.
# Linux MinGW ships libkernel32.a, and GNU ld on a case-sensitive host requires
# the library argument to match that lowercase filename.
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  arg="${args[i]}"
  if [[ "$arg" == "-l" ]]; then
    next=$((i + 1))
    if ((next < ${#args[@]})) && [[ "${args[next],,}" == "kernel32" ]]; then
      args[next]="kernel32"
    fi
  elif [[ "$arg" == -l* ]]; then
    library="${arg#-l}"
    if [[ "${library,,}" == "kernel32" ]]; then
      args[i]="-lkernel32"
    elif [[ "${library,,}" == ":libkernel32.a" ]]; then
      args[i]="-l:libkernel32.a"
    fi
  elif [[ "$arg" == -Wl,* ]]; then
    arg="${arg//-lKernel32/-lkernel32}"
    arg="${arg//-lKERNEL32/-lkernel32}"
    args[i]="$arg"
  fi
done

exec "$linker" "${args[@]}"
