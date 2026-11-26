#!/usr/bin/env bash
# Build and package the gateway only; this never connects to the server.
set -euo pipefail
[[ $# -eq 1 ]] || { echo "Usage: $0 OUTPUT_DIRECTORY" >&2; exit 2; }
out="$(realpath -m -- "$1")"
[[ ! -e "$out" ]] || { echo "Output directory must be new" >&2; exit 2; }
project="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$project"
target=x86_64-unknown-linux-gnu
dir="$(realpath -m -- "${CARGO_TARGET_DIR:-target}")"
cargo fmt --check
cargo test --locked -j "${BUILD_JOBS:-2}"
cargo build --locked --release --target "$target" --bin better-dot-net --target-dir "$dir" -j "${BUILD_JOBS:-2}"
binary="$dir/$target/release/better-dot-net"
LC_ALL=C readelf -h "$binary" | grep -q 'Advanced Micro Devices X86-64'
versions="$(LC_ALL=C readelf -V "$binary" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sort -Vu)"
max="${versions##*$'\n'}"
[[ -n "$max" && "$(printf '%s\n' "$max" GLIBC_2.34 | sort -V | tail -1)" == GLIBC_2.34 ]] || {
    echo "The x86-64 candidate must use glibc 2.34 or older; configure a compatible linker." >&2
    exit 1
}
umask 077
mkdir -m 0700 -- "$out"
install -m 0700 -- "$binary" "$out/better-dot-net"
install -m 0600 -- web/app.js deploy/install-candidate.sh "$out"
(
    cd -- "$out"
    sha256sum better-dot-net app.js install-candidate.sh > SHA256SUMS
    sha256sum -c SHA256SUMS
    tar --sort=name --mtime='@0' --owner=0 --group=0 -cf - better-dot-net app.js install-candidate.sh SHA256SUMS |
        gzip -n > candidate.tgz
    sha256sum candidate.tgz > candidate.tgz.sha256
)
printf 'Candidate written: %s (%s maximum)\n' "$out" "$max"
