#!/usr/bin/env python3
"""Issue one short-lived code for an enrolled, granted non-owner Agent."""
import argparse
import hashlib
import os
from pathlib import Path
import pwd
import secrets
import stat
import sys
import tempfile
import time

from enroll import ID, open_private, private, read_config


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("node")
    parser.add_argument("--minutes", type=int, default=720)
    parser.add_argument("--config-dir", type=Path, default=Path("/etc/better-dot-net-advanced"))
    parser.add_argument("--invite-dir", type=Path, default=Path("/var/lib/better-dot-net-advanced/invites"))
    parser.add_argument("--user", default="bdn-collab")
    args = parser.parse_args()
    if os.geteuid() != 0 or not ID.fullmatch(args.node) or not 1 <= args.minutes <= 1440:
        parser.error("requires root, a valid node ID and a lifetime of 1–1440 minutes")
    os.umask(0o077)
    uid = pwd.getpwnam(args.user)
    fd = os.open(args.config_dir, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        private(fd, stat.S_ISDIR)
        config = read_config(fd)
        node = args.node
        if node == config["owner"] or node not in config["nodes"]:
            parser.error("requires an enrolled non-owner node")
        with open_private(fd, f"node-{node}.token", os.O_RDONLY) as token_fd:
            with os.fdopen(os.dup(token_fd), "rb") as stream:
                token = stream.read(128).rstrip(b"\n")
    finally:
        os.close(fd)
    if hashlib.sha256(token).hexdigest() != config["nodes"][node].lower():
        parser.error("node token does not match the enrolled digest")
    inv_fd = os.open(args.invite_dir, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        inv_stat = os.fstat(inv_fd)
        if inv_stat.st_uid not in (0, uid.pw_uid) or inv_stat.st_mode & 0o077:
            raise ValueError("invite directory must be private (mode 0700) and owned by root or service user")
        try:
            os.stat(f".claimed-{node}", dir_fd=inv_fd, follow_symlinks=False)
            parser.error("the node has already received its key; enroll a new node")
        except FileNotFoundError:
            pass
    finally:
        os.close(inv_fd)
    code = secrets.token_hex(32)
    destination = args.invite_dir / hashlib.sha256(code.encode("ascii")).hexdigest()
    with tempfile.NamedTemporaryFile(dir=args.invite_dir) as staged:
        staged.write(f"{int(time.time()) + args.minutes * 60}\n".encode("ascii") + token + b"\n")
        staged.flush()
        os.fchown(staged.fileno(), uid.pw_uid, uid.pw_gid)
        os.fsync(staged.fileno())
        os.link(staged.name, destination)
    # Only the authorized operator receives the one-time code; never print a node token.
    print(code)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError) as e:
        sys.exit(f"invite: {e}")
