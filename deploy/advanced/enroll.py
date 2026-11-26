#!/usr/bin/env python3
"""Administer the Advanced authentication directory without touching project state."""

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
import re
import secrets
import stat
import sys


CONFIG = "nodes.json"
LOCK = ".bdn-enroll.lock"
MAX_CONFIG = 16 * 1024
MAX_NODES = 64  # Including the owner; match bdn-collab's startup checks.
ID = re.compile(r"[a-zA-Z0-9_-]{1,64}")
DIGEST = re.compile(r"[0-9a-fA-F]{64}")


class Invalid(ValueError):
    pass


def private(fd, kind):
    s = os.fstat(fd)
    if (
        not kind(s.st_mode)
        or s.st_uid != os.geteuid()
        or s.st_mode & 0o077
        or (kind is stat.S_ISREG and s.st_nlink != 1)
    ):
        raise Invalid("requires a private, operator-owned directory and regular files")


@contextmanager
def open_private(directory, name, flags, mode=0o600):
    fd = os.open(name, flags | os.O_NOFOLLOW | os.O_NONBLOCK, mode, dir_fd=directory)
    try:
        private(fd, stat.S_ISREG)
        yield fd
    finally:
        os.close(fd)


def unique_pairs(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise Invalid("duplicate key in node directory")
        obj[key] = value
    return obj


def valid_string(value, pattern):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def read_config(directory):
    with open_private(directory, CONFIG, os.O_RDONLY) as fd:
        with os.fdopen(os.dup(fd), "rb") as stream:
            raw = stream.read(MAX_CONFIG + 1)
    if len(raw) > MAX_CONFIG:
        raise Invalid("node directory too large")
    try:
        cfg = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_pairs)
    except (UnicodeError, json.JSONDecodeError, RecursionError) as exc:
        raise Invalid("invalid node directory JSON") from exc
    if not isinstance(cfg, dict) or set(cfg) != {"owner", "nodes"}:
        raise Invalid("expected owner and nodes in the node directory")
    owner, nodes = cfg["owner"], cfg["nodes"]
    if (
        not valid_string(owner, ID)
        or not isinstance(nodes, dict)
        or not 1 <= len(nodes) <= MAX_NODES
        or owner not in nodes
        or any(not valid_string(n, ID) or not valid_string(d, DIGEST) for n, d in nodes.items())
        or len({d.lower() for d in nodes.values()}) != len(nodes)
    ):
        raise Invalid("invalid identities or credential digests in the node directory")
    return cfg


def exists(directory, name):
    try:
        os.stat(name, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return False
    return True


def write_new(directory, name, content):
    with open_private(directory, name, os.O_WRONLY | os.O_CREAT | os.O_EXCL) as fd:
        with os.fdopen(os.dup(fd), "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
    os.fsync(directory)


def replace_config(directory, cfg):
    encoded = (json.dumps(cfg, sort_keys=True, indent=2) + "\n").encode("ascii")
    if len(encoded) > MAX_CONFIG:
        raise Invalid("updated node directory too large")
    name = ".bdn-nodes-" + secrets.token_hex(16)
    # Never replace an existing temp path or follow a symlink.
    try:
        write_new(directory, name, encoded)
    except FileExistsError as exc:
        raise Invalid("temporary name collision; retry") from exc
    try:
        os.replace(name, CONFIG, src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
    finally:
        try:
            os.unlink(name, dir_fd=directory)
        except FileNotFoundError:
            pass


def operate(directory, command, node):
    cfg = read_config(directory)
    nodes = cfg["nodes"]
    if command == "list":
        for name in sorted(nodes):
            label = "owner" if name == cfg["owner"] else "identity"
            print(f"{name}\t{label}")
        return
    if not valid_string(node, ID):
        raise Invalid("node ID must contain 1–64 ASCII letters, digits, hyphens or underscores")
    token_name = f"node-{node}.token"
    retired_name = f".bdn-retired-{node}"
    if command == "add":
        if node in nodes:
            raise Invalid("node ID is already enrolled")
        if exists(directory, retired_name):
            raise Invalid("retired node IDs cannot be re-enrolled; choose a new ID")
        if len(nodes) >= MAX_NODES:
            raise Invalid("node directory is at capacity")
        token = secrets.token_hex(32).encode("ascii")
        digest = hashlib.sha256(token).hexdigest()
        if any(d.lower() == digest for d in nodes.values()):
            raise Invalid("generated digest collision; retry")
        # Make the token durable before exposing its digest. A crash may leave a
        # private, unauthenticated token but cannot install a digest without it.
        write_new(directory, token_name, token + b"\n")
        nodes[node] = digest
        replace_config(directory, cfg)
        print(f"{node}: authentication added on disk; token saved privately in {token_name}.")
    else:
        if node == cfg["owner"]:
            raise Invalid("the project owner cannot be removed")
        if node not in nodes:
            raise Invalid("node ID is not enrolled")
        if exists(directory, retired_name):
            # A previous interrupted removal may already have written its marker.
            with open_private(directory, retired_name, os.O_RDONLY):
                pass
        else:
            write_new(directory, retired_name, b"")
        del nodes[node]
        replace_config(directory, cfg)
        print(f"{node}: authentication removed on disk and node ID retired.")
    print("Restart the collaboration service to load changes. Project membership was unchanged.")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", default="/etc/better-dot-net-advanced",
                        help="private, operator-owned directory containing nodes.json")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("list", help="list configured identities, without digests or token values")
    sub.add_parser("add", help="generate a private token and add its digest")
    sub.add_parser("remove", help="remove a digest and permanently retire that node ID")
    for command in ("add", "remove"):
        sub.choices[command].add_argument("node")
    args = parser.parse_args(argv)
    os.umask(0o077)
    try:
        directory = os.open(args.directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            private(directory, stat.S_ISDIR)
            with open_private(directory, LOCK, os.O_RDWR | os.O_CREAT) as fd:
                fcntl.flock(fd, fcntl.LOCK_EX)
                operate(directory, args.command, getattr(args, "node", None))
        finally:
            os.close(directory)
    except (Invalid, OSError) as exc:
        print(f"enrollment: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
