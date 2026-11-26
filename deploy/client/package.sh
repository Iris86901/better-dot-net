#!/usr/bin/env bash
# Stage the standalone Agent CLI (`bdn`) release binary + SHA-256 checksum.
# Supports either a fully static musl binary or a glibc <= 2.34 x86-64 binary.
set -Eeuo pipefail
if [[ $# != 2 ]]; then
  echo "usage: bash deploy/client/package.sh BDN_RELEASE_BINARY OUTPUT_DIR" >&2
  exit 2
fi
bin=$(realpath -- "$1")
out=$(realpath -m -- "$2")
[[ -f "$bin" && -x "$bin" ]] || { echo "bdn binary not found or not executable: $bin" >&2; exit 2; }
arch=$(objdump -f "$bin")
[[ $arch == *elf64-x86-64* ]] || { echo "bdn is not Linux x86-64 ELF" >&2; exit 1; }
abi=$(objdump -T "$bin" 2>/dev/null | awk 'match($0,/GLIBC_[0-9.]+/) { print substr($0,RSTART+6,RLENGTH-6) }' | sort -V | tail -1 || true)
if [[ -n $abi && $(printf '%s\n' "$abi" '2.34' | sort -V | tail -1) != '2.34' ]]; then
  echo "bdn needs unsupported GNU libc ABI: $abi" >&2
  exit 1
fi
mkdir -p -- "$out"
install -m 0755 "$bin" "$out/bdn-linux-x86_64"
(
  cd "$out"
  sha256sum bdn-linux-x86_64 > bdn-linux-x86_64.sha256
)
echo "Agent client release: $out/bdn-linux-x86_64 (${abi:-static/musl})"
