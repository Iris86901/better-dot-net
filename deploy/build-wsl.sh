#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
target=x86_64-unknown-linux-musl
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-musl-gcc}"
cargo build --locked --release --target "$target" --jobs "${BUILD_JOBS:-2}"
install -Dm755 "${CARGO_TARGET_DIR:-target}/$target/release/better-dot-net" bin/better-dot-net
# Keep the old service's binary path for staged in-place upgrades.
install -Dm755 bin/better-dot-net bin/dot-proxy
file bin/better-dot-net
