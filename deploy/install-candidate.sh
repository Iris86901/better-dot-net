#!/usr/bin/env bash
# Deliberate root-only cut-over of the existing gateway. Does not deploy meow-rs,
# reload ingress, overwrite settings, or change the hostname.
set -euo pipefail
[[ EUID -eq 0 && $# -eq 3 ]] || {
    echo "Usage (administrator): bash $0 CANDIDATE_DIRECTORY BINARY_SHA256 APP_JS_SHA256" >&2
    exit 2
}
candidate=$(realpath -- "$1")
bin_sha=$2
app_sha=$3
[[ $bin_sha =~ ^[0-9a-f]{64}$ && $app_sha =~ ^[0-9a-f]{64}$ ]] || exit 2
root=/opt/dot-proxy
bin="$root/bin/dot-proxy"
app="$root/web/app.js"
service=dot-proxy.service
core=dot-proxy-core.service
umask 077
[[ -f "$bin" && ! -L "$bin" && -f "$app" && ! -L "$app" ]]
for f in better-dot-net app.js; do [[ -f "$candidate/$f" && ! -L "$candidate/$f" ]]; done
systemctl is-active --quiet "$service"
systemctl is-active --quiet "$core"
health() { [[ "$(curl -fsS --max-time 2 http://127.0.0.1:3030/health)" == ok ]]; }
health
backup=$(mktemp -d "$root/.rollback-XXXXXXXX")
cp -a -- "$bin" "$backup/dot-proxy"
cp -a -- "$app" "$backup/app.js"
bin_tmp="$root/bin/.candidate-${backup##*-}"
app_tmp="$root/web/.candidate-${backup##*-}"
cp -a -- "$bin" "$bin_tmp"
cp -a -- "$app" "$app_tmp"
cat -- "$candidate/better-dot-net" > "$bin_tmp"
cat -- "$candidate/app.js" > "$app_tmp"
printf '%s  %s\n' "$bin_sha" "$bin_tmp" "$app_sha" "$app_tmp" | sha256sum -c -
changed=0
recover() {
    local error=$?
    trap - EXIT
    if (( changed && error )); then
        echo 'Candidate health failed; restoring the old gateway and app.' >&2
        cp -a -- "$backup/dot-proxy" "$bin_tmp"
        cp -a -- "$backup/app.js" "$app_tmp"
        mv -f -- "$bin_tmp" "$bin"
        mv -f -- "$app_tmp" "$app"
        systemctl restart "$service"
        health || printf '%s\n' 'Restored files; verify gateway manually.' >&2
    fi
    rm -f -- "$bin_tmp" "$app_tmp"
    return "$error"
}
trap recover EXIT
changed=1
mv -f -- "$bin_tmp" "$bin"
mv -f -- "$app_tmp" "$app"
systemctl restart "$service"
for i in {1..15}; do
    if health; then break; fi
    sleep .3
done
health
systemctl is-active --quiet "$core"
printf 'Verified new gateway. Previous binaries retained in %s\n' "$backup"
