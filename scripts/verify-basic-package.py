"""Fresh Basic bundle smoke: direct mode, authorization, and rollback."""
import http.client
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.parse


def run(archive):
    with tempfile.TemporaryDirectory(prefix="bdn-basic-smoke-") as tmpdir:
        root = Path(tmpdir)
        with tarfile.open(archive) as bundle:
            bundle.extractall(root, filter="data")
        release = root / "basic"
        subprocess.run(["sha256sum", "-c", "--quiet", "SHA256SUMS"], cwd=release, check=True)
        assert not any(p.name in {"bdn-agent", "meow", "bdn-collab"} for p in release.rglob("*"))
        assert not (root / "state").exists()
        link = root / "current"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        key = secrets.token_hex(24)
        cookie_secret = secrets.token_hex(32)

        def activate(target):
            replacement = root / "pending"
            replacement.symlink_to(target, target_is_directory=True)
            os.replace(replacement, link)

        def request(method, route, body=None, headers=None):
            client = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            try:
                client.request(method, route, body, headers or {})
                r = client.getresponse()
                return r.status, dict(r.getheaders()), r.read()
            finally:
                client.close()

        def launch():
            return subprocess.Popen([str(link / "bin" / "better-dot-net")],
                                    env={"HOST":"127.0.0.1", "PORT":str(port),
                                         "STATE_DIR":str(root / "state"),
                                         "ACCESS_KEY":key, "COOKIE_SECRET":cookie_secret,
                                         "MEOW_BIN":str(root / "not-installed")},
                                    stdout=subprocess.DEVNULL)

        def ready(process):
            for _ in range(60):
                if process.poll() is not None:
                    break
                try:
                    code, _, body = request("GET", "/health")
                    if code == 200 and body.strip() == b"ok":
                        return
                except OSError:
                    pass
                time.sleep(.045)
            raise RuntimeError("browser gateway did not become healthy")

        def stop(process):
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)

        activate(release)
        process = launch()
        try:
            ready(process)
            assert request("GET", "/api/routing")[0] == 401
            assert request("GET", "/ws/", headers={
                "Connection": "Upgrade", "Upgrade": "websocket",
                "Sec-WebSocket-Version": "13",
                "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
            })[0] == 401
            form = urllib.parse.urlencode({"key": key})
            status, headers, _ = request("POST", "/auth", form, {
                "Content-Type":"application/x-www-form-urlencoded",
            })
            headers = {k.lower():v for k,v in headers.items()}
            assert status == 303 and "secure" in headers["set-cookie"].lower()
            session = {"Cookie": headers["set-cookie"].split(";",1)[0]}
            status, _, body = request("GET", "/api/routing", headers=session)
            assert status == 200 and b'"backend":"direct"' in body
            assert request("GET", "/v1/projects/fixture/messages", headers=session)[0] == 404
            status, _, body = request("GET", "/", headers=session)
            assert status == 200 and b"/app.js?v=" in body
        finally:
            stop(process)

        # An incomplete new release must fail to start. A rollback preserves state,
        # the browser cookie secret, and the previous complete asset tree.
        broken = root / "failed-release"
        broken.mkdir()
        activate(broken)
        try:
            process = launch()
        except FileNotFoundError:
            pass
        else:
            stop(process)
            raise AssertionError("incomplete release unexpectedly started")
        activate(release)
        process = launch()
        try:
            ready(process)
            assert request("GET", "/", headers=session)[0] == 200
            assert request("GET", "/api/routing", headers=session)[0] == 200
        finally:
            stop(process)
        print("PASS: empty-state Basic boot without meow or Advanced, auth checks and returning session after rollback")


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve())
