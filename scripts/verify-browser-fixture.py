"""Small loopback Chromium acceptance test; requires aiohttp and playwright."""
import asyncio
import itertools
import json
import os
import socket
import ssl
from pathlib import Path
import subprocess
import sys
import tempfile

from aiohttp import ClientSession, WSMsgType, web
from playwright.async_api import async_playwright
def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# Public-looking fixture constants are intercepted by the two local HTTP
# tunnels below. Neither the fixture nor gateway connects to those addresses.
TARGET = "http://1.1.1.1"
ALT = "http://8.8.8.8"
KEY = "fixture-only-access-key"
ROOT = Path(__file__).resolve().parents[1]
ids = itertools.count(1)
seen = []


async def fixture(reader, writer, route):
    ident = next(ids)
    try:
        head = await reader.readuntil(b"\r\n\r\n")
        allowed = (b"CONNECT 1.1.1.1:80 ", b"CONNECT 8.8.8.8:80 ")
        if not head.startswith(allowed):
            writer.write(b"HTTP/1.1 403 Unknown Fixture\r\n\r\n")
            await writer.drain()
            return
        writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        await writer.drain()
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            method, path, _ = head.split(b"\r\n", 1)[0].decode().split(" ", 2)
            assert method in {"GET", "HEAD"}
            seen.append((route, ident, path))
            typ = "text/html; charset=utf-8"
            if path.startswith("/probe"):
                typ = "application/json"
                body = json.dumps({"route": route, "connection": ident}).encode()
            elif path == "/detail":
                body = b'<!doctype html><title>Fixture Detail</title><h1 id="detail">Detail</h1>'
            else:
                body = b"""<!doctype html><title>Fixture Home</title>
                <h1>Fixture Home</h1><a id="detail-link" href="/detail">Open detail</a>
                <button id="spa" onclick="history.pushState({},'', '/spa'); this.textContent='SPA ready'">Open SPA</button>
                <a id="new" href="/detail" target="_blank">New tab detail</a>
                <script>if (!localStorage.getItem('fixture_login')) {
                  localStorage.setItem('fixture_login','kept');
                  document.cookie='fixture_login=kept; path=/';
                }</script>"""
            writer.write(
                f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\nContent-Type: {typ}\r\n"
                f"Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: keep-alive\r\n\r\n".encode()
                + (body if method == "GET" else b"")
            )
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    finally:
        writer.close()
        await writer.wait_closed()


def ip_tls(directory, address):
    root = Path(directory)
    nss = root / ".pki/nssdb"
    nss.mkdir(parents=True)
    ca, ca_key, leaf, leaf_key, req, ext = (root / n for n in
        ("ca.pem", "ca.key", "ip.pem", "ip.key", "ip.csr", "ip.ext"))
    ext.write_text(f"subjectAltName=IP:{address}\nextendedKeyUsage=serverAuth\n")
    commands = [
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-subj",
         "/CN=BDN local fixture CA", "-days", "1", "-keyout", str(ca_key), "-out", str(ca)],
        ["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj",
         f"/CN={address}", "-keyout", str(leaf_key), "-out", str(req)],
        ["openssl", "x509", "-req", "-in", str(req), "-CA", str(ca), "-CAkey",
         str(ca_key), "-CAcreateserial", "-days", "1", "-extfile", str(ext), "-out", str(leaf)],
        ["certutil", "-N", "--empty-password", "-d", f"sql:{nss}"],
        ["certutil", "-A", "-d", f"sql:{nss}", "-n", "BDN fixture only", "-t", "C,,", "-i", str(ca)],
    ]
    for cmd in commands:
        subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(leaf, leaf_key)
    return context


async def run(binary, meow, literal_ip_tls=False, root=ROOT):
    port, mixed, ctl = (free_port() for _ in range(3))
    gateway = f"http://127.0.0.1:{port}"
    fixture_a = await asyncio.start_server(lambda r,w: fixture(r,w,"A"), "127.0.0.1", 0)
    fixture_b = await asyncio.start_server(lambda r,w: fixture(r,w,"B"), "127.0.0.1", 0)
    endpoint = lambda server: f"http://127.0.0.1:{server.sockets[0].getsockname()[1]}"
    async with ClientSession() as http:
        async def reverse(req):
            if req.path.startswith("/ws"):
                upstream = await http.ws_connect(gateway + str(req.rel_url),
                                                 headers={"Cookie": req.headers.get("Cookie", "")})
                downstream = web.WebSocketResponse()
                await downstream.prepare(req)
                async def copy(src, dst):
                    async for msg in src:
                        if msg.type == WSMsgType.BINARY:
                            await dst.send_bytes(msg.data)
                        elif msg.type == WSMsgType.TEXT:
                            await dst.send_str(msg.data)
                jobs = [asyncio.create_task(copy(upstream, downstream)),
                        asyncio.create_task(copy(downstream, upstream))]
                await asyncio.wait(jobs, return_when=asyncio.FIRST_COMPLETED)
                for job in jobs:
                    job.cancel()
                await asyncio.gather(*jobs, return_exceptions=True)
                await upstream.close()
                await downstream.close()
                return downstream
            async with http.request(req.method, gateway + str(req.rel_url),
                                    data=await req.read(), allow_redirects=False,
                                    headers={k:v for k,v in req.headers.items()
                                             if k.lower() not in {"host","content-length","connection","accept-encoding"}}) as resp:
                return web.Response(status=resp.status, body=await resp.read(),
                                    headers={k:v for k,v in resp.headers.items()
                                             if k.lower() not in {"content-length","content-encoding","transfer-encoding","connection"}})

        async def static(req):
            response = web.FileResponse(root / "web" / req.match_info["name"])
            response.headers["Service-Worker-Allowed"] = "/"
            return response

        app = web.Application()
        app.router.add_static("/assets/", root / "web" / "assets")
        app.router.add_get("/{name:sw\\.js|app\\.js|lifecycle\\.js|styles\\.css|favicon\\.svg}", static)
        app.router.add_route("*", "/{tail:.*}", reverse)
        runner = web.AppRunner(app)
        await runner.setup()
        frontend = free_port()
        with tempfile.TemporaryDirectory() as state:
            tls = ip_tls(state, "127.0.0.1") if literal_ip_tls else None
            await web.TCPSite(runner, "127.0.0.1", frontend, ssl_context=tls).start()
            base = f"{'https' if tls else 'http'}://127.0.0.1:{frontend}"
            core = None
            core_log = Path(state, "core.log")
            child = subprocess.Popen([str(binary)], env={
                **os.environ, "HOST":"127.0.0.1", "PORT":str(port),
                "STATE_DIR":state, "ACCESS_KEY":KEY,
                "UPSTREAM_PROXY":"", "UPSTREAM_SOCKS5":"",
                "MEOW_BIN":str(meow), "MEOW_SOCKS":f"127.0.0.1:{mixed}",
                "MEOW_API":f"127.0.0.1:{ctl}", "MEOW_SECRET":"fixture-only",
            }, stdout=subprocess.DEVNULL)
            try:
                for _ in range(60):
                    try:
                        async with http.get(gateway + "/health") as resp:
                            if resp.status == 200:
                                break
                    except OSError:
                        pass
                    await asyncio.sleep(.05)
                else:
                    raise RuntimeError("fixture gateway did not start")
                with core_log.open("wb") as log:
                    core = subprocess.Popen(
                        [str(meow), "--no-external-plugins", "-d", state, "-f", str(Path(state, "meow.yaml"))],
                        stdout=log, stderr=subprocess.STDOUT,
                        env={**os.environ, "HOME":state, "XDG_CACHE_HOME":state},
                    )
                for _ in range(50):
                    try:
                        async with http.get(f"http://127.0.0.1:{ctl}/version",
                                            headers={"Authorization":"Bearer fixture-only"}) as resp:
                            if resp.status == 200:
                                break
                    except OSError:
                        pass
                    await asyncio.sleep(.05)
                else:
                    raise RuntimeError("meow fixture did not start")
                async with http.get(gateway + "/?key=" + KEY, allow_redirects=False) as res:
                    assert res.status == 303
                    cookie = res.headers["Set-Cookie"].split(";", 1)[0]
                nodes = endpoint(fixture_a) + "#A\n" + endpoint(fixture_b) + "#B"
                async with http.post(gateway + "/api/routing", headers={"Cookie":cookie}, json={
                    "mode":"upstream", "config":nodes, "selected_node":"A",
                }) as res:
                    assert res.status == 200, await res.text()
                async with async_playwright() as pw:
                    executable = os.environ.get("CHROMIUM", "/usr/bin/chromium")
                    browser = await pw.chromium.launch(executable_path=None if executable == "bundled" else executable,
                                                       args=["--no-sandbox","--disable-gpu"],
                                                       env={**os.environ, **({"HOME":state,"XDG_CONFIG_HOME":state} if tls else {})})
                    if tls:
                        negative = await browser.new_page()
                        await negative.route("http://192.0.2.10/", lambda route: route.fulfill(
                            status=200, content_type="text/html", body="<title>Isolated insecure IP origin</title>"))
                        await negative.goto("http://192.0.2.10/")
                        assert await negative.evaluate("!isSecureContext && !('serviceWorker' in navigator)")
                        await negative.close()
                    ctx = await browser.new_context()
                    page = await ctx.new_page()
                    errors = []
                    page.set_default_timeout(5000)
                    page.on("pageerror", lambda err: errors.append(str(err)))
                    active_frame = page.frame_locator(".tab-frame-pane.active iframe")
                    if os.environ.get("BDN_VERIFY_PROFILES"):
                        await page.goto(f"{base}/?key={KEY}")
                        await page.get_by_text("Ready", exact=True).wait_for(timeout=10000)
                        await page.get_by_role("button", name="Pasted Nodes").click()
                        await page.locator("#routing-config-input").wait_for(state="visible")
                        await page.locator("#routing-node-select").select_option("B")
                        await page.wait_for_function("""async () => {
                          const response = await fetch('/api/routing');
                          return (await response.json()).upstream.active_node === 'B';
                        }""")
                        await page.get_by_role("button", name="Remote Clash").click()
                        await page.get_by_label("Clash URL").wait_for(state="visible")
                        assert not await page.locator("#routing-config-input").is_visible()
                        await page.get_by_role("button", name="Pasted Nodes").click()
                        await page.locator("#routing-node-select").select_option("A")
                        await page.wait_for_function("""async () => {
                          const response = await fetch('/api/routing');
                          return (await response.json()).upstream.active_node === 'A';
                        }""")
                    await page.goto(f"{base}/?key={KEY}&url={TARGET}/")
                    await active_frame.get_by_text("Fixture Home", exact=True).wait_for(timeout=12000)
                    assert await page.evaluate("isSecureContext && !!navigator.serviceWorker.controller")
                    assert await page.evaluate("location.hostname === '127.0.0.1'")
                    frame = next(f for f in page.frames if "/p/" in f.url)
                    async def probe(f, name):
                        return await f.evaluate("async path => (await fetch(path,{cache:'no-store'})).json()", name)
                    old = await probe(frame, "/probe/old")
                    assert old["route"] == "A"
                    result = await page.evaluate("""async () => (await fetch('/api/routing', {
                        method:'POST', headers:{'Content-Type':'application/json'},
                        body:JSON.stringify({mode:'upstream',selected_node:'B'})
                    })).json()""")
                    assert result["backend"] == "meow-rs", result
                    same_origin = await probe(frame, "/probe/reused")
                    absolute_alt = await probe(frame, ALT + "/probe/fresh")
                    assert absolute_alt["route"] == "B", (old, same_origin, absolute_alt, seen)
                    assert await frame.evaluate("localStorage.getItem('fixture_login')") == "kept"
                    assert "fixture_login=kept" in (await frame.evaluate("document.cookie"))
                    await frame.locator("#spa").click()
                    await frame.get_by_text("SPA ready").wait_for(timeout=3500)
                    await frame.locator("#detail-link").click()
                    await active_frame.get_by_text("Detail", exact=True).wait_for(timeout=4500)
                    await page.locator("#btn-back").click()
                    await active_frame.get_by_text("Fixture Home", exact=True).wait_for(timeout=5500)
                    assert await active_frame.locator("#dot-connection-failure").count() == 0
                    await page.locator("#btn-fwd").click()
                    await active_frame.get_by_text("Detail", exact=True).wait_for(timeout=5500)
                    await page.locator("#btn-new-tab").click()
                    await page.locator("#hero-input").fill(ALT + "/background")
                    await page.locator("#hero-input").press("Enter")
                    await active_frame.get_by_text("Fixture Home", exact=True).wait_for(timeout=5000)
                    await page.locator(".tab-item").first.click()
                    await active_frame.get_by_text("Detail", exact=True).wait_for(timeout=5000)
                    await page.reload(wait_until="domcontentloaded")
                    await active_frame.get_by_text("Detail", exact=True).wait_for(timeout=10000)
                    assert await page.locator(".tab-item").count() == 2
                    assert await page.locator(".tab-frame-pane iframe").count() == 1
                    frame = next(f for f in page.frames if "/p/" in f.url)
                    assert await frame.evaluate("localStorage.getItem('fixture_login')") == "kept"
                    await page.locator(".tab-item").nth(1).click()
                    await active_frame.get_by_text("Fixture Home", exact=True).wait_for(timeout=5000)
                    assert await page.locator(".tab-frame-pane iframe").count() == 2
                    print(f"OK: Chromium one-link, switch, warm={same_origin}, fresh={absolute_alt}; SPA, link, tab reload and lazy tab recovery")
                    await browser.close()
            except Exception:
                print(f"Fixture attempts: {seen}; browser errors: {locals().get('errors',[])}", file=sys.stderr)
                if core_log.exists():
                    print(core_log.read_text(), file=sys.stderr)
                raise
            finally:
                if core is not None:
                    core.terminate()
                    core.wait(timeout=4)
                child.terminate()
                child.wait(timeout=4)
                await runner.cleanup()
                fixture_a.close()
                fixture_b.close()


if __name__ == "__main__":
    options = sys.argv[3:]
    webroot = Path(options[options.index("--release") + 1]).resolve() if "--release" in options else ROOT
    asyncio.run(run(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve(), "--https-ip" in options, webroot))
