"""Real loopback HTTP smoke for the opt-in collaboration daemon."""
import concurrent.futures
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time


def run(binary, cli=None):
    with tempfile.TemporaryDirectory(prefix="bdn-collab-http-") as tmp:
        path = Path(tmp)
        tokens = {n: secrets.token_hex(32) for n in ("owner", "a", "b", "c", "outsider")}
        config = path / "trusted-nodes.json"
        config.write_text(json.dumps({
            "owner":"owner",
            "nodes":{n: hashlib.sha256(t.encode()).hexdigest() for n,t in tokens.items()},
        }))
        config.chmod(0o600)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]

        credentials = {}
        if cli:
            for name, token in tokens.items():
                cred = path / (name + ".token")
                cred.touch(mode=0o600, exist_ok=False)
                cred.write_text(token + "\n")
                credentials[name] = cred

        def terminal(node, *parts, ok=True, addr=None, tls=()):
            result = subprocess.run(
                [cli, *tls, addr or f"127.0.0.1:{port}", str(credentials[node]), *map(str, parts)],
                capture_output=True, timeout=8,
            )
            assert (result.returncode == 0) == ok, result.stderr
            return result

        def check_tls(node, expected_seq):
            def make_cert(stem):
                cert, key = path / (stem + ".pem"), path / (stem + ".key")
                ca, cakey = path / (stem + "-ca.pem"), path / (stem + "-ca.key")
                csr, exts = path / (stem + ".csr"), path / (stem + ".ext")
                exts.write_text("basicConstraints=critical,CA:FALSE\n"
                                "keyUsage=critical,digitalSignature\n"
                                "authorityKeyIdentifier=keyid,issuer\n"
                                "extendedKeyUsage=serverAuth\n"
                                "subjectAltName=DNS:collab.test,DNS:localhost\n")
                def openssl(*args):
                    subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True, timeout=8)
                openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
                        "-nodes", "-days", "1", "-subj", f"/CN={stem} CA", "-addext",
                        "basicConstraints=critical,CA:TRUE", "-addext",
                        "keyUsage=critical,keyCertSign,cRLSign", "-keyout", cakey, "-out", ca)
                openssl("req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
                        "-nodes", "-subj", "/CN=collab.test", "-keyout", key, "-out", csr)
                openssl("x509", "-req", "-in", csr, "-CA", ca, "-CAkey", cakey, "-CAcreateserial",
                        "-days", "1", "-extfile", exts, "-out", cert)
                return str(cert), str(key), str(ca)

            cert, key, trusted_ca = make_cert("correct")
            _, _, wrong_cert = make_cert("untrusted")
            observed = []

            class FixedFixture(BaseHTTPRequestHandler):
                def do_POST(self):
                    assert self.path == "/v1/action"
                    observed.append(self.path)
                    body = self.rfile.read(int(self.headers["Content-Length"]))
                    if json.loads(body).get("after") == 999:
                        self.send_response(302)
                        self.send_header("Location", "/never-follow")
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    backend = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    try:
                        backend.request("POST", self.path, body, {
                            "Authorization": self.headers["Authorization"],
                            "Content-Type": "application/json",
                        })
                        reply = backend.getresponse()
                        data = reply.read()
                        self.send_response(reply.status)
                        self.send_header("Content-Length", str(len(data)))
                        self.end_headers()
                        self.wfile.write(data)
                    finally:
                        backend.close()

                def log_message(self, *args):
                    pass

            with ThreadingHTTPServer(("127.0.0.1", 0), FixedFixture) as server:
                context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                context.load_cert_chain(cert, key)
                server.socket = context.wrap_socket(server.socket, server_side=True)
                worker = threading.Thread(target=server.serve_forever, daemon=True)
                worker.start()
                addr = f"127.0.0.1:{server.server_port}"
                endpoint = f"https://localhost:{server.server_port}"
                try:
                    result = terminal(node, "messages", 0, 5, addr=addr, tls=("--tls", "collab.test", trusted_ca))
                    assert json.loads(result.stdout)["messages"][0]["seq"] == expected_seq
                    for host, ca in (("wrong.test", trusted_ca), ("collab.test", wrong_cert)):
                        failure = terminal(node, "messages", ok=False, addr=addr, tls=("--tls", host, ca))
                        assert b"TLS peer verification failed" in failure.stderr
                    result = terminal(node, "messages", 0, 5, addr=endpoint, tls=("--https", "--ca", trusted_ca))
                    assert json.loads(result.stdout)["messages"][0]["seq"] == expected_seq
                    failures = (
                        (endpoint, wrong_cert, b"TLS peer verification failed"),
                        (f"https://127.0.0.1:{server.server_port}", trusted_ca,
                         b"TLS peer verification failed"),
                        (f"http://localhost:{server.server_port}", cert, b"requires https://"),
                        (endpoint + "/other", cert, b"authority only"),
                        (endpoint.replace("localhost", "example@localhost"), cert, b"authority only"),
                    )
                    for remote, ca, message in failures:
                        failure = terminal(node, "messages", ok=False, addr=remote,
                                           tls=("--https", "--ca", ca))
                        assert message in failure.stderr, failure.stderr
                    failure = terminal(node, "messages", 999, 5, ok=False, addr=endpoint,
                                       tls=("--https", "--ca", trusted_ca))
                    assert b"HTTP 302" in failure.stderr
                    assert observed == ["/v1/action"] * 3

                    profile = path / "dot-state"
                    def dot(*parts, ok=True):
                        result = subprocess.run(
                            [cli, *map(str, parts)], timeout=8, capture_output=True,
                            env={**os.environ, "BDN_HOME": str(profile), "SSL_CERT_FILE": trusted_ca},
                        )
                        assert (result.returncode == 0) == ok, result.stderr
                        return result

                    dot("setup", endpoint, credentials["outsider"], ok=False)
                    assert not profile.exists()
                    assert not dot("setup", endpoint, credentials[node]).stdout
                    saved_cfg = profile / "config.json"
                    saved_cred = profile / "credentials"
                    original_cfg = saved_cfg.read_bytes()
                    original_cred = saved_cred.read_bytes()
                    assert saved_cfg.stat().st_mode & 0o777 == 0o600
                    assert saved_cred.stat().st_mode & 0o777 == 0o600
                    assert profile.stat().st_mode & 0o777 == 0o700
                    dot("setup", endpoint, credentials["outsider"], ok=False)
                    assert saved_cfg.read_bytes() == original_cfg
                    assert saved_cred.read_bytes() == original_cred
                    credentials[node].rename(path / "old-source.token")
                    try:
                        for _ in range(2):
                            result = dot("messages", 0, 5)
                            assert json.loads(result.stdout)["messages"][0]["seq"] == expected_seq
                    finally:
                        (path / "old-source.token").rename(credentials[node])
                finally:
                    server.shutdown()
                    worker.join(timeout=8)

            caddy = os.environ.get("BDN_TEST_CADDY")
            if caddy:
                with socket.socket() as sock:
                    sock.bind(("127.0.0.1", 0))
                    edge_port = sock.getsockname()[1]
                template = (Path(__file__).resolve().parents[1] /
                            "deploy/advanced/Caddyfile").read_text()
                edge_conf = path / "Caddyfile"
                edge_conf.write_text(
                    "{\n\tadmin off\n\tauto_https disable_redirects\n"
                    "\tservers {\n\t\tprotocols h1\n\t}\n}\n" +
                    template.replace("collab.example.com {",
                                     f"https://localhost:{edge_port} {{\n\tbind 127.0.0.1\n\ttls {cert} {key}")
                    .replace("127.0.0.1:9300", f"127.0.0.1:{port}")
                )
                with (path / "caddy.log").open("w+") as edge_log:
                    edge_env = {"PATH": os.defpath, "HOME": str(path),
                                "XDG_CONFIG_HOME": str(path), "XDG_DATA_HOME": str(path)}
                    edge = subprocess.Popen([caddy, "run", "--config", str(edge_conf), "--adapter", "caddyfile"],
                                            env=edge_env, stdout=edge_log, stderr=edge_log)
                    try:
                        context = ssl.create_default_context(cafile=trusted_ca)
                        until = time.monotonic() + 10
                        last_error = None
                        while time.monotonic() < until:
                            probe = http.client.HTTPSConnection("localhost", edge_port, timeout=1, context=context)
                            try:
                                probe.request("GET", "/health")
                                assert probe.getresponse().status == 404
                                break
                            except ssl.SSLError:
                                raise
                            except OSError as exc:
                                last_error = exc
                                if edge.poll() is not None:
                                    raise RuntimeError("Caddy HTTPS fixture exited") from exc
                                time.sleep(.05)
                            finally:
                                probe.close()
                        else:
                            raise RuntimeError("Caddy HTTPS fixture did not become ready") from last_error
                        url = f"https://localhost:{edge_port}"
                        flags = ("--https", "--ca", trusted_ca)
                        terminal("a", "create", "edge-build", "Build edge fixture", "b", addr=url, tls=flags)
                        ids = json.loads(terminal("b", "tasks", addr=url, tls=flags).stdout)["tasks"]
                        task_id = next(item["id"] for item in ids if item["detail"] == "Build edge fixture")
                        lease = json.loads(terminal("b", "claim", task_id, addr=url, tls=flags).stdout)["lease"]
                        upload, download = path / "edge-input.bin", path / "edge-output.bin"
                        upload.write_bytes(bytes(range(256)) * 4096)
                        terminal("b", "upload", "edge-output", upload, addr=url, tls=flags)
                        terminal("b", "complete", task_id, lease, "done", "--artifact", "edge-output",
                                 addr=url, tls=flags)
                        terminal("a", "download", "edge-output", download, addr=url, tls=flags)
                        assert download.read_bytes() == upload.read_bytes()
                        status = json.loads(terminal("a", "task", task_id, addr=url, tls=flags).stdout)["status"]
                        assert status["complete"]["artifact"]["sha256"] == hashlib.sha256(upload.read_bytes()).hexdigest()
                        assert b"HTTP 403" in terminal("outsider", "tasks", ok=False, addr=url, tls=flags).stderr
                        print("PASS: Caddy HTTPS routes authenticated task discovery, claim, verified handoff and artifact download")
                    except Exception:
                        edge_log.seek(0)
                        print(edge_log.read()[-4000:], file=sys.stderr)
                        raise
                    finally:
                        edge.terminate()
                        edge.wait(timeout=5)

        def request(node, method, url, data=None):
            headers = {}
            if node in tokens:
                headers["Authorization"] = "Bearer " + tokens[node]
            if isinstance(data, dict):
                data = json.dumps(data).encode()
                headers["Content-Type"] = "application/json"
            elif data is not None:
                headers["Content-Type"] = "application/octet-stream"
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            try:
                c.request(method, url, data, headers)
                r = c.getresponse()
                return r.status, r.read()
            finally:
                c.close()

        def call(identity, op, expected=200, **args):
            status, b = request(identity,"POST","/v1/action",{"action":op, **args})
            assert status == expected, (identity, op, status, b)
            return json.loads(b)

        def start():
            proc = subprocess.Popen(
                [binary, str(config), str(path / "state"), f"127.0.0.1:{port}"],
                stdout=subprocess.DEVNULL,
            )
            for _ in range(50):
                try:
                    if request(None, "GET", "/health") == (200, b"ok"):
                        return proc
                except OSError:
                    pass
                if proc.poll() is not None:
                    raise RuntimeError("daemon exited during boot")
                time.sleep(.05)
            proc.kill()
            proc.wait(timeout=3)
            raise RuntimeError("daemon did not become healthy")

        p = start()
        try:
            call(None, "messages", expected=401, limit=5)
            call("outsider", "messages", expected=403, limit=5)
            call("a", "grant", expected=403, node="a")
            call("owner", "grant", node="a")
            call("owner", "grant", node="b")
            call("owner", "grant", node="c")
            call("a", "create_task", id="assigned-a", detail="For B", assigned="b")
            call("a", "create_task", id="assigned-z", detail="Also for B", assigned="b")
            page = call("b", "tasks", limit=1)
            assert [t["id"] for t in page["tasks"]] == ["assigned-a"]
            assert page["next_after"] == "assigned-a"
            assert [t["id"] for t in call("b", "tasks", after=page["next_after"])["tasks"]] == ["assigned-z"]
            assert call("c", "tasks")["tasks"] == []
            call("c", "task", id="assigned-a", expected=403)
            call("c", "claim", id="assigned-a", seconds=30, expected=403)
            call("outsider", "tasks", expected=403)
            if cli:
                result = terminal("b", "tasks", "--after", "assigned-a", "--limit", "1")
                assert [t["id"] for t in json.loads(result.stdout)["tasks"]] == ["assigned-z"]
            seq = call("a", "post", request_id="start", body="Build requested")["seq"]
            assert call("a", "post", request_id="start", body="Build requested")["seq"] == seq
            if cli:
                check_tls("b", seq)
            call("a", "create_task", id="build", detail="Build this project", assigned=None)
            def claim(node):
                return node, request(node,"POST","/v1/action",{"action":"claim","id":"build","seconds":120})
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                claims = dict(pool.map(claim, ("a","b")))
            assert sorted(v[0] for v in claims.values()) == [200,403]
            winner = next(n for n,v in claims.items() if v[0] == 200)
            loser = "a" if winner=="b" else "b"
            lease = json.loads(claims[winner][1])["lease"]
            call(loser,"complete",expected=403,id="build",lease=lease,result="incorrect")
            content = os.urandom(1024*1024+19)
            code, blob = request(winner,"PUT","/v1/artifact/build-output",content)
            assert code==200 and json.loads(blob)["sha256"]==hashlib.sha256(content).hexdigest()
            call(winner,"complete",expected=403,id="build",lease=lease,
                 result="not-published",artifact="missing")
            if cli:
                terminal(winner, "complete", "build", lease, "build-output", "--artifact", "build-output")
            else:
                call(winner,"complete",id="build",lease=lease,result="build-output",artifact="build-output")
            # Hold two genuine partial uploads: a third transfer gets bounded backpressure
            # while the other node can still read project messages.
            def partial(name):
                c = socket.create_connection(("127.0.0.1", port), timeout=5)
                c.sendall((
                    f"PUT /v1/artifact/{name} HTTP/1.1\r\n"
                    f"Host: 127.0.0.1:{port}\r\n"
                    f"Authorization: Bearer {tokens[winner]}\r\n"
                    "Content-Length: 16777216\r\n\r\n"
                ).encode() + b"x" * (192 * 1024))
                return c

            with partial("interrupted-a"), partial("interrupted-b"):
                blob_dir = path / "state" / "blobs"
                for _ in range(60):
                    if sum(f.stat().st_size > 0 for f in blob_dir.glob(".upload.*")) == 2:
                        break
                    time.sleep(.025)
                else:
                    raise AssertionError("staged streams did not reach disk")
                assert request(loser, "GET", "/v1/artifact/build-output")[0] == 503
                assert call(loser, "messages", after=0, limit=5)["messages"][0]["seq"] == seq
                # Model a synced blob linked without its project metadata.
                orphan = blob_dir / hashlib.sha256(b"orphan fixture").hexdigest()
                orphan.write_bytes(b"orphan fixture")
                p.kill()
                p.wait(timeout=3)
            p = start()
            assert not list(blob_dir.glob(".upload.*"))
            assert not orphan.exists()
            assert request(loser, "GET", "/v1/artifact/interrupted-a")[0] == 403
            assert call(loser,"messages",after=0,limit=5)["messages"][0]["seq"]==seq
            completed = call(loser,"task",id="build")["status"]["complete"]
            assert completed["result"] == "build-output"
            assert completed["artifact"] == {
                "name": "build-output", "bytes": len(content), "sha256": hashlib.sha256(content).hexdigest(),
            }
            assert request(loser,"GET","/v1/artifact/build-output")== (200,content)
            assert request("outsider","GET","/v1/artifact/build-output")[0]==403
            call("owner","revoke",node=loser)
            assert request(loser,"GET","/v1/artifact/build-output")[0]==403
            call(loser,"messages",expected=403,limit=5)
            if cli:
                assert b"HTTP 403" in terminal(loser, "messages", ok=False).stderr
        finally:
            if p.poll() is None:
                p.terminate()
                p.wait(timeout=3)

        bad = subprocess.run([binary,str(config),str(path/"state"),"0.0.0.0:0"],capture_output=True)
        assert bad.returncode != 0 and b"loopback only" in bad.stderr
    if cli:
        print("PASS: Rust CLI task/message flow, artifact equality, private credentials, no overwrite and HTTPS certificate checks")
    print("PASS: isolated two-node auth, exclusive claims, bytes, revocation, backpressure, and interrupted-upload recovery")


if __name__ == "__main__":
    run(*(str(Path(arg).resolve()) for arg in sys.argv[1:]))
