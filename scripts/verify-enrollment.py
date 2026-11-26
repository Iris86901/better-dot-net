#!/usr/bin/env python3
"""Exercise private enrollment, optionally against the real loopback daemon."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import tempfile
import time


TOOL = Path(__file__).resolve().parent.parent / "deploy" / "advanced" / "enroll.py"
OWNER = "01" * 32  # Fixture value, never accepted by a deployed instance.


def digest(token):
    return hashlib.sha256(token).hexdigest()


def config(directory):
    return json.loads((directory / "nodes.json").read_text())


def initialize(directory):
    file = directory / "nodes.json"
    file.write_text(json.dumps({"owner": "owner", "nodes": {"owner": digest(OWNER.encode())}}))
    file.chmod(0o600)
    return file


def invoke(directory, *args, ok=True):
    ran = subprocess.run([sys.executable, str(TOOL), "--directory", str(directory), *args],
                         capture_output=True, timeout=8)
    assert (ran.returncode == 0) == ok, (args, ran.returncode, ran.stderr)
    return ran.stdout + ran.stderr


def disk_checks(base):
    directory = base / "private"
    directory.mkdir(mode=0o700)
    path = initialize(directory)
    assert invoke(directory, "list") == b"owner\towner\n"

    for invalid in ("../a", "x.y", "语", "", "x" * 65):
        invoke(directory, "add", "--", invalid, ok=False)

    with ThreadPoolExecutor(max_workers=4) as pool:
        jobs = [pool.submit(invoke, directory, "add", f"agent-{n}") for n in range(8)]
        outputs = [job.result() for job in jobs]

    nodes = config(directory)["nodes"]
    assert len(nodes) == 9
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert stat.S_IMODE((directory / ".bdn-enroll.lock").stat().st_mode) == 0o600
    for n, output in enumerate(outputs):
        ident = f"agent-{n}"
        token_file = directory / f"node-{ident}.token"
        value = token_file.read_bytes().strip()
        assert re.fullmatch(rb"[0-9a-f]{64}", value)
        assert stat.S_IMODE(token_file.stat().st_mode) == 0o600
        assert digest(value) == nodes[ident]
        assert value not in output and value not in path.read_bytes()
        assert nodes[ident].encode() not in invoke(directory, "list")
    assert len(set(nodes.values())) == 9

    existing = (directory / "node-agent-0.token").read_bytes()
    invoke(directory, "add", "agent-0", ok=False)
    invoke(directory, "remove", "owner", ok=False)
    assert config(directory)["nodes"] == nodes
    invoke(directory, "remove", "agent-0")
    assert "agent-0" not in config(directory)["nodes"]
    assert (directory / "node-agent-0.token").read_bytes() == existing
    (directory / "node-agent-0.token").unlink()
    invoke(directory, "add", "agent-0", ok=False)
    assert stat.S_IMODE((directory / ".bdn-retired-agent-0").stat().st_mode) == 0o600

    orphan = directory / "node-orphan.token"
    orphan.write_text("do-not-overwrite")
    orphan.chmod(0o600)
    invoke(directory, "add", "orphan", ok=False)
    assert orphan.read_text() == "do-not-overwrite"
    assert "orphan" not in config(directory)["nodes"]
    symlink_token = directory / "node-systems.token"
    symlink_token.symlink_to(orphan)
    invoke(directory, "add", "systems", ok=False)
    assert orphan.read_text() == "do-not-overwrite"
    assert "systems" not in config(directory)["nodes"]

    # Refuse unsafe configurations before mutation; the caller must protect files.
    raw = path.read_bytes()
    path.chmod(0o644)
    invoke(directory, "list", ok=False)
    path.chmod(0o600)
    directory.chmod(0o755)
    invoke(directory, "list", ok=False)
    directory.chmod(0o700)
    alias = base / "alias"
    alias.symlink_to(directory, target_is_directory=True)
    invoke(alias, "list", ok=False)
    path.unlink()
    target = base / "target"
    target.write_bytes(raw)
    target.chmod(0o600)
    path.symlink_to(target)
    invoke(directory, "list", ok=False)
    assert target.read_bytes() == raw
    path.unlink()
    os.link(target, path)
    invoke(directory, "list", ok=False)
    path.unlink()
    path.write_bytes(b'{"owner":"a","owner":"b","nodes":{}}')
    path.chmod(0o600)
    invoke(directory, "list", ok=False)
    path.write_text(json.dumps({"owner": "owner", "nodes": {
        "owner": "A" * 64, "duplicate": "a" * 64,
    }}))
    invoke(directory, "list", ok=False)
    path.write_bytes(b" " * (16 * 1024 + 1))
    invoke(directory, "list", ok=False)

    print("PASS: protected files, digest-only directory, locking, validation, and no ID reuse")


def call(port, token, action):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        client.request("POST", "/v1/action", json.dumps(action),
                       {"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        response = client.getresponse()
        response.read()
        return response.status
    finally:
        client.close()


@contextmanager
def daemon(binary, directory, port):
    proc = subprocess.Popen([binary, str(directory / "nodes.json"), str(directory / "state"),
                             f"127.0.0.1:{port}"], stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE)
    try:
        for _ in range(75):
            if proc.poll() is not None:
                raise AssertionError(proc.stderr.read().decode())
            try:
                if call(port, OWNER, {"action": "messages", "after": 0, "limit": 10}) == 200:
                    break
            except (OSError, http.client.HTTPException):
                time.sleep(0.04)
        else:
            raise AssertionError("loopback daemon did not become ready")
        yield
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3)
        proc.stderr.close()


def integration(base, binary):
    directory = base / "live"
    directory.mkdir(mode=0o700)
    initialize(directory)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]

    read = {"action": "messages", "after": 0, "limit": 10}
    with daemon(binary, directory, port):
        invoke(directory, "add", "collaborator")
        token = (directory / "node-collaborator.token").read_text().strip()
        assert call(port, token, read) == 401  # Credential config was already loaded.
    with daemon(binary, directory, port):
        assert call(port, token, read) == 403  # Authentic but no project membership.
        assert call(port, OWNER, {"action": "grant", "node": "collaborator"}) == 200
        assert call(port, token, read) == 200
        assert call(port, OWNER, {"action": "revoke", "node": "collaborator"}) == 200
        assert call(port, token, read) == 403
        invoke(directory, "remove", "collaborator")
        assert call(port, token, read) == 403
    with daemon(binary, directory, port):
        assert call(port, token, read) == 401
    print("PASS: the real daemon separates credential reload (401) from live membership (403)")


def main():
    if len(sys.argv) > 2:
        raise SystemExit("usage: python3 scripts/verify-enrollment.py [BDN_COLLAB_BINARY]")
    with tempfile.TemporaryDirectory(prefix="bdn-enrollment-") as path:
        base = Path(path)
        disk_checks(base)
        if len(sys.argv) == 2:
            integration(base, str(Path(sys.argv[1]).resolve()))


if __name__ == "__main__":
    main()
