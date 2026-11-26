"""Exercise a newly extracted Basic release with real Caddy, TLS and (optional) Chromium."""
import base64
import hashlib
import http.client
import os
from pathlib import Path
import re
import secrets
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.parse


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=4)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def browser_check(origin, cert, key, assets):
    from playwright.sync_api import expect, sync_playwright

    pem = subprocess.check_output(["openssl", "x509", "-in", str(cert), "-pubkey", "-noout"])
    der = subprocess.check_output(["openssl", "pkey", "-pubin", "-outform", "DER"], input=pem)
    pin = base64.b64encode(hashlib.sha256(der).digest()).decode("ascii")
    with sync_playwright() as pw:
        browser = pw.chromium.launch(args=["--ignore-certificate-errors-spki-list=" + pin])
        try:
            page = browser.new_page()
            page.goto(origin + "/", wait_until="domcontentloaded")
            page.locator('input[name="key"]').fill(key)
            page.locator('input[name="key"]').press("Enter")
            expect(page.locator("#status-text")).to_have_text("Ready", timeout=12000)
            assert page.evaluate("navigator.serviceWorker.controller !== null")
            page.reload(wait_until="domcontentloaded")
            expect(page.locator("#status-text")).to_have_text("Ready", timeout=12000)
            for tag, attr, name in (("script", "src", "app.js"), ("link", "href", "styles.css")):
                path = page.locator(f'{tag}[{attr}^="/{name}?"]').get_attribute(attr)
                assert path and re.fullmatch(rf"/{re.escape(name)}\?v=[1-9][0-9]*", path), path
                digest = page.evaluate("""async (path) => {
                    const response = await fetch(path);
                    if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
                    const hash = await crypto.subtle.digest("SHA-256", await response.arrayBuffer());
                    return Array.from(new Uint8Array(hash), n => n.toString(16).padStart(2, "0")).join("");
                }""", path)
                assert digest == hashlib.sha256((assets / name).read_bytes()).hexdigest(), path
            page.get_by_role("button", name="Remote Clash").click()
            expect(page.get_by_role("button", name="Preview", exact=True)).to_have_count(0)
            expect(page.get_by_role("button", name="Use Selected Node")).to_be_disabled()
            expect(page.get_by_role("button", name="Delete", exact=True)).to_be_hidden()
            link = page.get_by_label("Clash URL")
            link.fill("http://subscription.example.com/config.yaml")
            link.press("Enter")
            expect(page.locator("#routing-msg")).to_contain_text("Enter an HTTPS")
            expect(page.get_by_role("button", name="Remote Clash")).to_have_class(re.compile(r"\bactive\b"))
            expect(page.get_by_role("button", name="Direct VPS")).not_to_have_class(re.compile(r"\bactive\b"))
            expect(page.locator("#meow-badge")).to_have_text("Direct VPS")
            page.get_by_role("button", name="Direct VPS").click()
            expect(page.get_by_role("button", name="Direct VPS")).to_have_class(re.compile(r"\bactive\b"))
            assert page.evaluate("""() => new Promise(resolve => {
                const ws = new WebSocket('wss://' + location.host + '/ws/');
                const timer = setTimeout(() => { ws.close(); resolve(false); }, 3500);
                ws.onmessage = () => { clearTimeout(timer); ws.close(); resolve(true); };
                ws.onerror = () => { clearTimeout(timer); resolve(false); };
            })""")
            print("PASS: Chromium login, service-worker control, bundled assets, empty Clash panel and authenticated WSS")
        finally:
            browser.close()


def run(release, caddy, browser):
    subprocess.run(["sha256sum", "-c", "--quiet", "SHA256SUMS"], cwd=release, check=True)
    gateway_port, ingress_port = free_port(), free_port()
    if gateway_port == ingress_port:
        raise RuntimeError("fixture assigned the same port twice")
    key = secrets.token_hex(24)
    with tempfile.TemporaryDirectory(prefix="bdn-basic-tls-") as scratch:
        root = Path(scratch)
        cert, pkey = root / "cert.pem", root / "key.pem"
        subprocess.run([
            "openssl", "req", "-x509", "-nodes", "-newkey", "rsa:2048",
            "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost",
            "-keyout", str(pkey), "-out", str(cert),
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        trust = ssl.create_default_context(cafile=str(cert))
        config = root / "Caddyfile"
        source = (release / "Caddyfile").read_text()
        config.write_text(
            "{\n    admin off\n    auto_https disable_redirects\n}\n" +
            source.replace("proxy.example.com {",
                           f"https://localhost:{ingress_port} {{\n    bind 127.0.0.1\n    tls {cert} {pkey}")
                  .replace("/opt/better-dot-net-basic/current/web", str(release / "web"))
                  .replace("127.0.0.1:3030", f"127.0.0.1:{gateway_port}")
        )
        env = {"PATH": os.defpath, "HOME":scratch, "XDG_CONFIG_HOME":scratch, "XDG_DATA_HOME":scratch}
        bin_path = str(release / "bin/better-dot-net")
        meow = str(release / "bin/meow")
        mixed, ctl = free_port(), free_port()
        state = root / "state"
        env.update({"HOST":"127.0.0.1", "PORT":str(gateway_port), "STATE_DIR":str(state),
                    "ACCESS_KEY":key, "MEOW_BIN":meow, "MEOW_SOCKS":f"127.0.0.1:{mixed}",
                    "MEOW_API":f"127.0.0.1:{ctl}", "MEOW_SECRET":secrets.token_hex(32),
                    "BDN_SUBSCRIPTION_HOSTS":"subscription.example.com",
                    "REQUIRE_MEOW":"1"})
        subprocess.run([bin_path, "--prepare-meow"], env=env, check=True)
        edge = gw = core = None
        try:
            core = subprocess.Popen([meow, "--no-external-plugins", "-d", str(state),
                                     "-f", str(state / "meow.yaml")], env=env, stdout=subprocess.DEVNULL)
            gw = subprocess.Popen([bin_path], env=env, stdout=subprocess.DEVNULL)
            with (root / "caddy.log").open("wb") as log:
                edge = subprocess.Popen([caddy, "run", "--config", str(config), "--adapter", "caddyfile"],
                                        env=env, stdout=log, stderr=subprocess.STDOUT)
            def req(method, path, body=None, headers=None, upgraded=False):
                conn = http.client.HTTPSConnection("localhost", ingress_port, context=trust, timeout=2)
                try:
                    conn.request(method, path, body, headers or {})
                    reply = conn.getresponse()
                    return reply.status, {k.lower():v for k,v in reply.getheaders()}, (b"" if upgraded else reply.read())
                finally:
                    conn.close()
            for _ in range(80):
                if edge.poll() is not None or gw.poll() is not None or core.poll() is not None:
                    raise RuntimeError("isolated Caddy or gateway exited")
                try:
                    if req("GET", "/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("isolated HTTPS ingress did not become ready")
            status, headers, code = req("GET", "/sw.js")
            assert status == 200 and code and headers["service-worker-allowed"] == "/"
            assert headers["cache-control"] == "no-cache"
            status, headers, body = req("GET", "/assets/rt/engine.wasm")
            assert status == 200 and body[:4] == b"\x00asm"
            assert req("GET", "/api/routing")[0] == 401
            upgrade = {
                "Connection":"Upgrade", "Upgrade":"websocket", "Sec-WebSocket-Version":"13",
                "Sec-WebSocket-Key":"dGhlIHNhbXBsZSBub25jZQ==",
            }
            assert req("GET", "/ws/", headers=upgrade)[0] == 401
            status, headers, _ = req("POST", "/auth", urllib.parse.urlencode({"key":key}),
                                    {"Content-Type":"application/x-www-form-urlencoded"})
            assert status == 303 and "secure" in headers["set-cookie"].lower()
            cookie = {"Cookie":headers["set-cookie"].split(";",1)[0]}
            assert req("GET","/api/routing",headers=cookie)[0] == 200
            assert req("GET","/ws/",headers=upgrade|cookie,upgraded=True)[0] == 101
            print("PASS: verified TLS, packaged SW/WASM, browser cookie and authenticated WSS through fresh loopback Caddy")
            if browser:
                browser_check(f"https://localhost:{ingress_port}", cert, key, release / "web")
        except Exception:
            log = root / "caddy.log"
            if log.exists():
                print(log.read_text()[-2500:], file=sys.stderr)
            raise
        finally:
            if edge is not None:
                stop(edge)
            if gw is not None:
                stop(gw)
            if core is not None:
                stop(core)


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve(), str(Path(sys.argv[2]).resolve()), "--browser" in sys.argv[3:])
