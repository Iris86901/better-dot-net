#!/usr/bin/env bash
# Stage a complete browser-only release; never touches system paths or services.
set -euo pipefail
[[ $# -eq 3 ]] || { echo "Usage: $0 X86_64_RELEASE_BINARY NEW_OUTPUT_DIRECTORY MEOW_RELEASE_ARCHIVE" >&2; exit 2; }
bin=$(realpath -- "$1")
out=$(realpath -m -- "$2")
meow_archive=$(realpath -- "$3")
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
[[ -f "$bin" && -x "$bin" && ! -e "$out" ]] || exit 2
echo "92fe902cca5e95799af02a42fcda9a4f9195aa61ec126057b51b15a8d2116ca2  $meow_archive" | sha256sum -c --status
LC_ALL=C readelf -h "$bin" | grep 'Advanced Micro Devices X86-64' >/dev/null
versions=$(LC_ALL=C readelf -V "$bin" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sort -Vu)
abi="${versions##*$'\n'}"
[[ -n "$abi" && "$(printf '%s\n' "$abi" GLIBC_2.34 | sort -V | tail -1)" == GLIBC_2.34 ]] || {
    echo "Release needs x86-64 Linux glibc 2.34 or older" >&2; exit 1;
}
mkdir -m 0700 -- "$out"
stage="$out/basic"
mkdir -p -- "$stage"/{bin,licenses,web/assets/rt}
install -m 0755 -- "$bin" "$stage/bin/better-dot-net"
meow_path=meow-v0.29.0-x86_64-unknown-linux-musl
tar xOzf "$meow_archive" "$meow_path/meow" > "$stage/bin/meow"
chmod 0755 "$stage/bin/meow"
tar xOzf "$meow_archive" "$meow_path/LICENSE" > "$stage/licenses/meow-rs-LICENSE"
install -m 0644 -- "$repo"/deploy/basic/{README.md,Caddyfile,better-dot-net-basic.service,better-dot-net-basic-core.service,better-dot-net-basic.sysusers} "$stage"
for f in app.js sw.js styles.css favicon.svg lifecycle.js assets/rt/engine.js assets/rt/engine.wasm assets/rt/controller.api.js assets/rt/controller.inject.js assets/rt/controller.sw.js assets/rt/transport.js; do
    install -m 0644 -- "$repo/web/$f" "$stage/web/$f"
done
(
    cd "$stage"
    LC_ALL=C find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum > SHA256SUMS
    chmod 0644 SHA256SUMS
    sha256sum -c SHA256SUMS
)
(
    cd "$out"
    LC_ALL=C tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner -cf - basic | gzip -n > basic-linux-x86_64.tgz
    sha256sum basic-linux-x86_64.tgz > basic-linux-x86_64.tgz.sha256
)
echo "Browser-only release: $out/basic-linux-x86_64.tgz ($abi maximum)"
