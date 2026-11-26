"""End-to-end verification of high-performance reverse service exposure over multiplexed WSS."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time

DOMAIN = "expose.test"


def start_local_app(tag):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/big":
                payload = (tag.encode() + b"-" * 60) * 16384  # ~1 MiB
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            body = json.dumps({
                "tag": tag,
                "path": self.path,
                "host": self.headers.get("Host"),
                "xfh": self.headers.get("X-Forwarded-Host"),
                "xfp": self.headers.get("X-Forwarded-Proto"),
                "cookie": self.headers.get("Cookie"),
                "auth": self.headers.get("Authorization"),
                "bdn_tok": self.headers.get("X-BDN-Token"),
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=srv.serve_forever, daemon=True)
    worker.start()
    return srv, srv.server_port


def verify(server, cli):
    with tempfile.TemporaryDirectory(prefix="bdn-expose-") as tmp:
        root = Path(tmp)
        keys = {name: secrets.token_hex(32) for name in ("owner", "dot_a", "dot_b")}
        directory = root / "nodes.json"
        directory.touch(mode=0o600)
        directory.write_text(json.dumps({
            "owner": "owner",
            "nodes": {n: hashlib.sha256(t.encode()).hexdigest() for n, t in keys.items()},
        }))
        cred_files = {}
        for name, tok in keys.items():
            p = root / f"{name}.token"
            p.touch(mode=0o600)
            p.write_text(tok + "\n")
            cred_files[name] = p

        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            proxy_port = sock.getsockname()[1]
        addr = f"127.0.0.1:{port}"

        env = {
            **os.environ,
            "BDN_EXPOSE_DOMAIN": DOMAIN,
            "BDN_EGRESS_ENABLED": "1",
            "BDN_EGRESS_ALLOW_LOOPBACK_TEST": "1",
        }
        daemon = subprocess.Popen(
            [server, str(directory), str(root / "state"), addr],
            env=env,
            stdout=subprocess.DEVNULL,
        )

        app1, port1 = start_local_app("app1")
        app2, port2 = start_local_app("app2")
        egress_target, egress_port = start_local_app("egress-target")
        tunnel_proc = None

        def http_call(method, path, host=None, body=None, **headers):
            hdrs = dict(headers)
            if host:
                hdrs["Host"] = host
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                conn.request(method, path, body, hdrs)
                resp = conn.getresponse()
                return resp.status, dict((k.lower(), v) for k, v in resp.getheaders()), resp.read()
            finally:
                conn.close()

        def run_cli(node, *args, ok=True):
            res = subprocess.run(
                [cli, addr, str(cred_files[node]), *map(str, args)],
                capture_output=True,
                timeout=10,
            )
            assert (res.returncode == 0) == ok, (node, args, res.returncode, res.stderr, res.stdout)
            return res

        try:
            for _ in range(40):
                try:
                    if http_call("GET", "/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                raise RuntimeError("bdn-collab did not start")

            run_cli("owner", "grant", "dot_a")
            run_cli("owner", "grant", "dot_b")

            # Reserved subdomains cannot be published.
            assert run_cli("dot_a", "service", "publish", "collab", port1, ok=False).returncode != 0
            assert run_cli("dot_a", "service", "publish", "dot", port1, ok=False).returncode != 0

            # Start ONE persistent multiplexed tunnel for dot_a with BOTH inbound expose and outbound local proxy.
            tunnel_proc = subprocess.Popen(
                [
                    cli, addr, str(cred_files["dot_a"]), "tunnel",
                    "--allow", f"{port1},{port2}",
                    "--proxy", f"127.0.0.1:{proxy_port}",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

            # Publish two services on dot_a: app-priv (default private) & app-pub (public).
            pub1 = json.loads(run_cli("dot_a", "service", "publish", "app-priv", port1).stdout)
            assert pub1["access"] == "private" and pub1["url"] == f"https://app-priv.{DOMAIN}"
            pub2 = json.loads(run_cli("dot_a", "service", "publish", "app-pub", port2, "--public").stdout)
            assert pub2["access"] == "public" and pub2["url"] == f"https://app-pub.{DOMAIN}"

            # Wait until dot_a's tunnel is marked online.
            for _ in range(40):
                listing = json.loads(run_cli("dot_a", "service", "list").stdout)
                if all(s["online"] for s in listing["services"]):
                    break
                time.sleep(0.05)
            else:
                raise RuntimeError("dot_a tunnel did not connect")

            # Global uniqueness: dot_b cannot hijack or overwrite dot_a's subdomain.
            conflict = run_cli("dot_b", "service", "publish", "app-priv", port1, ok=False)
            assert b"HTTP 409" in conflict.stderr

            # Default private service rejects anonymous callers and accepts granted node Bearer.
            assert http_call("GET", "/hello", host=f"app-priv.{DOMAIN}")[0] == 401
            st, _, data = http_call(
                "GET", "/hello?a=1", host=f"app-priv.{DOMAIN}",
                Authorization="Bearer " + keys["dot_b"],
            )
            assert st == 200
            info = json.loads(data)
            assert info["tag"] == "app1" and info["path"] == "/hello?a=1"
            assert info["xfh"] == f"app-priv.{DOMAIN}" and info["xfp"] == "https"
            # Internal cluster Bearer token must be stripped before reaching Dot's local app.
            assert info["auth"] is None

            # Public service works anonymously, and control-plane routes are NOT exposed on it!
            st, _, data = http_call("GET", "/v1/action", host=f"app-pub.{DOMAIN}")
            assert st == 200 and json.loads(data)["tag"] == "app2"

            # Concurrent multiplexing: 8 parallel requests across both services including 1 MiB payloads.
            def fetch(i):
                if i % 2 == 0:
                    s, _, b = http_call("GET", "/big", host=f"app-pub.{DOMAIN}")
                    assert s == 200 and len(b) == len((b"app2" + b"-" * 60) * 16384)
                else:
                    s, _, b = http_call("GET", f"/p/{i}", host=f"app-pub.{DOMAIN}")
                    assert s == 200 and json.loads(b)["path"] == f"/p/{i}"

            with ThreadPoolExecutor(max_workers=8) as pool:
                list(pool.map(fetch, range(8)))

            # Switch app-priv dynamically to token mode (no tunnel restart!).
            tok = "my-super-secret-token-123"
            updated = json.loads(run_cli("dot_a", "service", "access", "app-priv", "--token", tok).stdout)
            assert updated["access"] == "token"
            assert http_call("GET", "/", host=f"app-priv.{DOMAIN}")[0] == 401
            assert http_call("GET", "/", host=f"app-priv.{DOMAIN}", **{"X-BDN-Token": "wrong-token"})[0] == 401
            st, _, data = http_call("GET", "/api", host=f"app-priv.{DOMAIN}", **{"X-BDN-Token": tok})
            assert st == 200 and json.loads(data)["bdn_tok"] is None

            # Browser ?bdn_token= login sets __Host-bdn-svc cookie and strips token from URL.
            st, hdrs, _ = http_call("GET", f"/ui?bdn_token={tok}&view=2", host=f"app-priv.{DOMAIN}")
            assert st == 302 and hdrs["location"] == "/ui?view=2"
            cookie_pair = hdrs["set-cookie"].split(";", 1)[0]
            assert cookie_pair.startswith("__Host-bdn-svc=")
            st, _, data = http_call(
                "GET", "/ui?view=2", host=f"app-priv.{DOMAIN}",
                Cookie=f"{cookie_pair}; app_sess=xyz",
            )
            assert st == 200
            info = json.loads(data)
            assert info["cookie"] == "app_sess=xyz"

            # Outbound transparent local proxy (HTTP forward + HTTP CONNECT + SOCKS5) over the SAME tunnel!
            # 1) Plain HTTP forward proxy
            pconn = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=8)
            try:
                pconn.request("GET", f"http://127.0.0.1:{egress_port}/via-http-proxy")
                presp = pconn.getresponse()
                assert presp.status == 200
                pinfo = json.loads(presp.read())
                assert pinfo["tag"] == "egress-target" and pinfo["path"] == "/via-http-proxy"
            finally:
                pconn.close()

            # 2) HTTP CONNECT tunnel
            csock = socket.create_connection(("127.0.0.1", proxy_port), timeout=8)
            try:
                csock.sendall(f"CONNECT 127.0.0.1:{egress_port} HTTP/1.1\r\nHost: 127.0.0.1:{egress_port}\r\n\r\n".encode())
                reply = b""
                while b"\r\n\r\n" not in reply:
                    reply += csock.recv(1024)
                assert reply.startswith(b"HTTP/1.1 200 ")
                csock.sendall(b"GET /via-connect HTTP/1.1\r\nHost: egress.local\r\nConnection: close\r\n\r\n")
                raw = b""
                while True:
                    chunk = csock.recv(4096)
                    if not chunk:
                        break
                    raw += chunk
                assert b'"via-connect"' in raw or b'"/via-connect"' in raw
            finally:
                csock.close()

            # 3) SOCKS5 proxy
            ssock = socket.create_connection(("127.0.0.1", proxy_port), timeout=8)
            try:
                ssock.sendall(b"\x05\x01\x00")
                assert ssock.recv(2) == b"\x05\x00"
                ssock.sendall(b"\x05\x01\x00\x01\x7f\x00\x00\x01" + egress_port.to_bytes(2, "big"))
                srep = ssock.recv(10)
                assert srep[:2] == b"\x05\x00"
                ssock.sendall(b"GET /via-socks5 HTTP/1.1\r\nHost: egress.local\r\nConnection: close\r\n\r\n")
                raw = b""
                while True:
                    chunk = ssock.recv(4096)
                    if not chunk:
                        break
                    raw += chunk
                assert b'"/via-socks5"' in raw
            finally:
                ssock.close()

            # 4) Cloud metadata SSRF (169.254.169.254) must STILL be blocked by VPS policy!
            msock = socket.create_connection(("127.0.0.1", proxy_port), timeout=8)
            try:
                msock.sendall(b"CONNECT 169.254.169.254:80 HTTP/1.1\r\nHost: 169.254.169.254:80\r\n\r\n")
                mrep = msock.recv(1024)
                assert b"502 Bad Gateway" in mrep
            finally:
                msock.close()

            # Owner sees all nodes' services and can take down any node's service.
            owner_list = json.loads(run_cli("owner", "service", "list").stdout)
            assert {s["subdomain"] for s in owner_list["services"]} == {"app-priv", "app-pub"}
            run_cli("owner", "service", "remove", "app-pub")
            assert http_call("GET", "/", host=f"app-pub.{DOMAIN}")[0] == 404
            assert http_call("GET", "/api", host=f"app-priv.{DOMAIN}", **{"X-BDN-Token": tok})[0] == 200

            # Revoking dot_a kills its tunnel and removes remaining services immediately.
            run_cli("owner", "revoke", "dot_a")
            assert http_call("GET", "/api", host=f"app-priv.{DOMAIN}", **{"X-BDN-Token": tok})[0] == 404
            tunnel_proc.wait(timeout=8)
            assert tunnel_proc.returncode != 0
        finally:
            if tunnel_proc and tunnel_proc.poll() is None:
                tunnel_proc.terminate()
                tunnel_proc.wait(timeout=5)
            app1.shutdown()
            app2.shutdown()
            egress_target.shutdown()
            daemon.terminate()
            daemon.wait(timeout=5)
    print("PASS: wildcard subdomain routing, bidirectional WSS multiplexing (inbound expose + outbound HTTP/CONNECT/SOCKS5 proxy), uniqueness, private/public/token+cookie access, SSRF guard, owner takedown and revocation")


if __name__ == "__main__":
    verify(*(str(Path(p).resolve()) for p in sys.argv[1:]))
