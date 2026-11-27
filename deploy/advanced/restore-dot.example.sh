#!/bin/sh
# Example fixed startup entry for a private service. Review and configure once.
# The service binary must stay in the foreground, listen only on localhost,
# and not fork a second copy. OAuth/event payloads are never command input.
set -eu
umask 077
cd -- "$(dirname -- "$0")"
test -x ./bdn && test -x ./my-app && test -f ./credentials && test -f ./config.json
export BDN_HOME="$PWD"
command -v flock >/dev/null
command -v nohup >/dev/null
nohup flock -n -F .my-app.lock ./my-app >> my-app.log 2>&1 </dev/null &
nohup flock -n -F .bdn.lock ./bdn expose myapp:3000 --keep >> bdn.log 2>&1 </dev/null &
# This entry requests startup; confirm service health independently.
