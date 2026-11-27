#!/usr/bin/env bash
# Build a reproducible Advanced server archive from prebuilt Linux x86-64 release binaries.
# The standalone Agent client (`bdn`) is packaged independently by deploy/client/package.sh.
# Does not compile, install, start a service, or read any machine credentials.
set -Eeuo pipefail
if [[ $# != 2 ]]; then
  echo "usage: bash deploy/advanced/package.sh RELEASE_BINARY_DIR OUTPUT_DIR" >&2
  exit 2
fi
binaries=$1
out=$2
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for name in bdn-collab bdn-terminal; do
  file="$binaries/$name"
  arch=$(objdump -f "$file")
  [[ $arch == *elf64-x86-64* ]] || { echo "$name is not Linux x86-64 ELF" >&2; exit 1; }
  abi=$(objdump -T "$file" | awk 'match($0,/GLIBC_[0-9.]+/) { print substr($0,RSTART+6,RLENGTH-6) }' | sort -V | tail -1)
  if [[ -n $abi && $(printf '%s\n' "$abi" '2.34' | sort -V | tail -1) != '2.34' ]]; then
    echo "$name needs unsupported GNU libc ABI: $abi" >&2
    exit 1
  fi
done
mkdir -p -- "$out"
tmp=$(mktemp -d "$out/.stage.XXXXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
install -d "$tmp/tree/bin" "$tmp/tree/sbin" "$tmp/tree/licenses/oauth-as"
install -m 0755 "$binaries/bdn-collab" "$binaries/bdn-terminal" "$tmp/tree/bin/"
install -m 0644 "$here/../../vendor/oauth-as/LICENSE-MIT" "$here/../../vendor/oauth-as/LICENSE-APACHE" "$tmp/tree/licenses/oauth-as/"
install -m 0755 "$here/enroll.py" "$here/invite.py" "$tmp/tree/sbin/"
install -m 0644 "$here/bdn-collab.service" "$here/bdn-collab.sysusers" "$here/Caddyfile" "$here/ENROLLMENT.md" "$here/README.md" "$here/RECOVERY.zh-CN.md" "$here/AUTORECOVERY.md" "$here/restore-dot.example.sh" "$here/../terminal-plan.md" "$tmp/tree/"
(
  cd "$tmp/tree"
  sha256sum bin/bdn-collab bin/bdn-terminal sbin/enroll.py sbin/invite.py bdn-collab.service bdn-collab.sysusers Caddyfile ENROLLMENT.md README.md RECOVERY.zh-CN.md AUTORECOVERY.md restore-dot.example.sh terminal-plan.md licenses/oauth-as/LICENSE-MIT licenses/oauth-as/LICENSE-APACHE > SHA256SUMS
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - bin sbin licenses bdn-collab.service bdn-collab.sysusers Caddyfile ENROLLMENT.md README.md RECOVERY.zh-CN.md AUTORECOVERY.md restore-dot.example.sh terminal-plan.md SHA256SUMS |
    gzip -n > ../advanced-server-linux-x86_64.tgz
)
mv -f -- "$tmp/advanced-server-linux-x86_64.tgz" "$out/advanced-server-linux-x86_64.tgz"
(
  cd "$out"
  sha256sum advanced-server-linux-x86_64.tgz > advanced-server-linux-x86_64.tgz.sha256
)
echo "Advanced server release: $out/advanced-server-linux-x86_64.tgz"
