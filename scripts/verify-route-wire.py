"""Authenticated Wisp and mock upstreams with a real meow-rs binary.

Uses websockets when installed, or a loopback-only fixture client.
No public destination is dialed.
"""
import asyncio
import base64
from contextlib import asynccontextmanager
import hashlib
import http.client
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlsplit

try:
    from websockets.asyncio.client import connect as library_connect
except ImportError:
    library_connect = None

KEY = "fixture-only-access-key"
DEST = "1.1.1.1:443"  # Literal encoded in test frames; never dialed by fixtures.
STREAM_CHUNK = b"t" * 16384
STREAM_ROUNDS = 16


class FixtureSocket:
    """Binary, loopback-only client for targets without the test dependency."""
    def __init__(self, reader, writer):
        self.reader, self.writer = reader, writer

    async def send(self, payload, opcode=2):
        n = len(payload)
        header = bytes([128 | opcode])
        if n < 126:
            header += bytes([128 | n])
        elif n <= 65535:
            header += b"\xfe" + struct.pack("!H", n)
        else:
            header += b"\xff" + struct.pack("!Q", n)
        mask = os.urandom(4)
        self.writer.write(header + mask + bytes(c ^ mask[i % 4] for i,c in enumerate(payload)))
        await self.writer.drain()

    async def recv(self):
        body = bytearray()
        while True:
            a,b = await self.reader.readexactly(2)
            assert not (a & 112 or b & 128), "unexpected extension or server mask"
            size = b & 127
            if size == 126:
                size = struct.unpack("!H", await self.reader.readexactly(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", await self.reader.readexactly(8))[0]
            assert size + len(body) <= 1024 * 1024, "fixture frame limit"
            data = await self.reader.readexactly(size)
            opcode = a & 15
            if opcode == 9:
                await self.send(data, 10)
            elif opcode == 10:
                continue
            elif opcode in (0, 2):
                body.extend(data)
                if a & 128:
                    return bytes(body)
            else:
                raise RuntimeError(f"unexpected fixture opcode {opcode}")


@asynccontextmanager
async def connect(url, *, additional_headers, proxy):
    if library_connect is not None and not os.environ.get("BDN_BENCH_STDLIB"):
        async with library_connect(url, additional_headers=additional_headers, proxy=proxy) as ws:
            yield ws
        return
    parsed = urlsplit(url)
    assert parsed.scheme == "ws" and parsed.hostname == "127.0.0.1" and parsed.port and proxy is None
    reader, writer = await asyncio.open_connection(parsed.hostname, parsed.port, limit=32768)
    try:
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (f"GET {parsed.path} HTTP/1.1\r\nHost: {parsed.netloc}\r\n"
                   f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                   f"Sec-WebSocket-Key: {key}\r\nCookie: {additional_headers['Cookie']}\r\n\r\n")
        writer.write(request.encode("ascii"))
        await writer.drain()
        raw = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 3)
        status, *lines = raw.decode("ascii").split("\r\n")
        assert status.startswith("HTTP/1.1 101 "), status
        headers = {k.lower(): v.strip() for k,v in (line.split(":", 1) for line in lines if ":" in line)}
        digest = hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")).digest()
        assert headers.get("sec-websocket-accept") == base64.b64encode(digest).decode("ascii")
        ws = FixtureSocket(reader, writer)
        yield ws
        await ws.send(struct.pack("!H", 1000), 8)
    finally:
        writer.close()
        await writer.wait_closed()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def packet(kind, stream, payload):
    return struct.pack("<BI", kind, stream) + payload


async def smoke(binary, meow):
    port, mixed, ctl = (free_port() for _ in range(3))

    async def proxy(reader, writer, marker):
        try:
            head = await reader.readuntil(b"\r\n\r\n")
            assert head.startswith(f"CONNECT {DEST} HTTP/1.1\r\n".encode()), head
            writer.write(b"HTTP/1.1 200 Established\r\n\r\n")
            await writer.drain()
            while byte := await reader.read(1):
                if byte == b"S":
                    for _ in range(STREAM_ROUNDS):
                        writer.write(STREAM_CHUNK)
                        await writer.drain()
                        await asyncio.sleep(0.009)
                    continue
                writer.write(marker + byte)
                await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    a = await asyncio.start_server(lambda r, w: proxy(r, w, b"A"), "127.0.0.1", 0)
    b = await asyncio.start_server(lambda r, w: proxy(r, w, b"B"), "127.0.0.1", 0)

    def api(method, path, payload=None, cookie=None):
        con = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
        h = {"Content-Type": "application/json"}
        if cookie:
            h["Cookie"] = cookie
        try:
            con.request(method, path, json.dumps(payload) if payload is not None else None, h)
            resp = con.getresponse()
            return resp.status, resp.getheader("Set-Cookie"), resp.read()
        finally:
            con.close()

    async def change(cookie, mode, config=None, node=None, details=False):
        payload = {"mode": mode}
        if config:
            payload["config"] = config
        if node:
            payload["selected_node"] = node
        started = time.perf_counter()
        code, _, raw = await asyncio.to_thread(api, "POST", "/api/routing", payload, cookie)
        data = json.loads(raw)
        assert code == 200 and data["mode"] == mode, (code, raw)
        assert data["backend"] == ("meow-rs" if mode != "direct" else "direct"), data
        return data if details else (time.perf_counter() - started) * 1000

    with tempfile.TemporaryDirectory() as scratch:
        log = Path(scratch, "meow.log")
        meow_proc = None
        child = subprocess.Popen(
            [str(binary)],
            env={**os.environ, "HOST": "127.0.0.1", "STATE_DIR": scratch,
                 "PORT": str(port), "ACCESS_KEY": KEY, "UPSTREAM_PROXY": "",
                 "UPSTREAM_SOCKS5": "", "MEOW_BIN":str(meow),
                 "MEOW_SOCKS":f"127.0.0.1:{mixed}", "MEOW_API":f"127.0.0.1:{ctl}",
                 "MEOW_SECRET":"only-fixture"},
            stdout=subprocess.DEVNULL,
        )
        try:
            for _ in range(50):
                try:
                    if (await asyncio.to_thread(api, "GET", "/health"))[0] == 200:
                        break
                except OSError:
                    pass
                await asyncio.sleep(0.06)
            else:
                raise RuntimeError(f"gateway startup failed: {child.poll()}")
            with log.open("wb") as output:
                meow_proc = subprocess.Popen(
                    [str(meow), "--no-external-plugins", "-d", scratch, "-f", str(Path(scratch, "meow.yaml"))],
                    stdout=output, stderr=subprocess.STDOUT,
                    env={**os.environ, "HOME":scratch, "XDG_CACHE_HOME":scratch},
                )
            def ctl_ready():
                con = http.client.HTTPConnection("127.0.0.1", ctl, timeout=1)
                try:
                    con.request("GET", "/version", headers={"Authorization":"Bearer only-fixture"})
                    res = con.getresponse()
                    return res.status == 200 and res.read()
                finally:
                    con.close()
            for _ in range(60):
                try:
                    if await asyncio.to_thread(ctl_ready):
                        break
                except OSError:
                    pass
                if meow_proc.poll() is not None:
                    raise RuntimeError(log.read_text())
                await asyncio.sleep(.06)
            else:
                raise RuntimeError(f"meow controller did not start: {log.read_text()}")
            assert (await asyncio.to_thread(api, "GET", "/api/routing"))[0] == 401
            status, header, _ = await asyncio.to_thread(api, "GET", "/?key=" + KEY)
            assert status == 303 and header
            cookie = header.split(";")[0]
            config = "\n".join(
                f"http://127.0.0.1:{server.sockets[0].getsockname()[1]}#{name}"
                for name, server in [("A", a), ("B", b)]
            )
            switch_a = await change(cookie, "upstream", config=config, node="A")
            async with connect(f"ws://127.0.0.1:{port}/ws/",
                               additional_headers={"Cookie": cookie}, proxy=None) as ws:
                async def read_data(stream):
                    while raw := await asyncio.wait_for(ws.recv(), 4):
                        kind, sid = struct.unpack_from("<BI", raw)
                        if kind == 2 and sid == stream:
                            return raw[5:]
                        assert kind != 4, raw

                async def open_stream(stream):
                    await ws.send(packet(1, stream, struct.pack("<BH", 1, 443) + b"1.1.1.1"))

                await open_stream(1)
                await ws.send(packet(2, 1, b"x"))
                assert await read_data(1) == b"Ax"

                switch_b = await change(cookie, "upstream", node="B")
                start_fresh = time.perf_counter()
                await open_stream(2)
                await ws.send(packet(2, 2, b"y"))
                assert await read_data(2) == b"By"
                fresh = (time.perf_counter() - start_fresh) * 1000
                await ws.send(packet(2, 1, b"z"))
                assert await read_data(1) == b"Az"

                switch_direct = await change(cookie, "direct")
                await ws.send(packet(2, 2, b"w"))
                assert await read_data(2) == b"Bw"
                assert json.loads((await asyncio.to_thread(api, "GET", "/api/routing", None, cookie))[2])["backend"] == "direct"
                await change(cookie, "upstream", node="A")
                await open_stream(3)
                await ws.send(packet(2, 3, b"d"))
                assert await read_data(3) == b"Ad"
                await ws.send(packet(2, 2, b"e"))
                assert await read_data(2) == b"Be"
                if os.environ.get("BDN_VERIFY_PROFILES"):
                    b_port = b.sockets[0].getsockname()[1]
                    a_port = a.sockets[0].getsockname()[1]
                    yaml = (f"proxies:\n- {{name: Clash-JP, type: http, server: 127.0.0.1, port: {b_port}}}\n"
                            f"- {{name: Clash-US, type: http, server: 127.0.0.1, port: {a_port}}}\n")
                    state = await change(cookie, "meow", config=yaml, node="Clash-JP", details=True)
                    assert state["meow"]["active_node"] == "Clash-JP"
                    assert state["upstream"]["active_node"] == "A"
                    await open_stream(4)
                    await ws.send(packet(2, 4, b"m"))
                    assert await read_data(4) == b"Bm"
                    await change(cookie, "meow", node="Clash-US")
                    await open_stream(6)
                    await ws.send(packet(2, 6, b"n"))
                    assert await read_data(6) == b"An"
                    state = await change(cookie, "upstream", node="A", details=True)
                    assert state["meow"]["active_node"] == "Clash-US"
                    await open_stream(5)
                    await ws.send(packet(2, 5, b"p"))
                    assert await read_data(5) == b"Ap"
                    status, _, raw = await asyncio.to_thread(api, "DELETE", "/api/routing?mode=meow", None, cookie)
                    assert status == 200
                    state = json.loads(raw)
                    assert state["mode"] == "upstream" and state["upstream"]["active_node"] == "A"
                    assert state["meow"]["nodes"] == []
                    print("PASS: pasted and Clash profiles switch distinct real meow routes and clear independently")
                checks = int(os.environ.get("BDN_BENCH_STREAMS", "0"))
                if not 0 <= checks <= 32:
                    raise ValueError("BDN_BENCH_STREAMS must be between 0 and 32")
                if checks:
                    samples = []
                    # Bound the fixture load. These samples exercise the actual
                    # Wisp + meow-rs data path and never change a live route.
                    for sid in range(10, 10 + checks):
                        started = time.perf_counter()
                        await open_stream(sid)
                        await ws.send(packet(2, sid, b"r"))
                        assert await read_data(sid) == b"Ar"
                        samples.append((time.perf_counter() - started) * 1000)
                    samples.sort()
                    def rank(p):
                        return samples[(p * checks + 99) // 100 - 1]
                    print(f"Isolated fresh-stream echo: successes={checks}/{checks}, "
                          f"p50={rank(50):.2f} ms, p95={rank(95):.2f} ms")
                    def process_stats(pid):
                        status = Path(f"/proc/{pid}/status").read_text()
                        rss = int(next(line.split()[1] for line in status.splitlines()
                                       if line.startswith("VmRSS:")))
                        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                        return rss, int(fields[11]) + int(fields[12])

                    pids = [child.pid, meow_proc.pid]
                    pre = [process_stats(pid) for pid in pids]
                    peak = [rss for rss, _ in pre]
                    total = 0
                    small_ms = None
                    small_at = None
                    await open_stream(50)
                    await ws.send(packet(2, 50, b"S"))
                    deadline = asyncio.get_running_loop().time() + 4
                    while total < len(STREAM_CHUNK) * STREAM_ROUNDS or small_ms is None:
                        remaining = deadline - asyncio.get_running_loop().time()
                        raw = await asyncio.wait_for(ws.recv(), remaining)
                        kind, sid = struct.unpack_from("<BI", raw)
                        assert kind != 4, (kind, sid)
                        if kind != 2:
                            continue
                        if sid == 50:
                            assert raw[5:] == b"t" * (len(raw) - 5)
                            total += len(raw) - 5
                            if small_at is None:
                                small_at = time.perf_counter()
                                await open_stream(51)
                                await ws.send(packet(2, 51, b"s"))
                            for i, pid in enumerate(pids):
                                peak[i] = max(peak[i], process_stats(pid)[0])
                        elif sid == 51:
                            assert raw[5:] == b"As"
                            assert total < len(STREAM_CHUNK) * STREAM_ROUNDS, "small request completed after the stream"
                            small_ms = (time.perf_counter() - small_at) * 1000
                    assert total == len(STREAM_CHUNK) * STREAM_ROUNDS
                    await ws.send(packet(4, 50, b"\x02"))
                    await ws.send(packet(4, 51, b"\x02"))
                    await asyncio.sleep(0.05)
                    post = [process_stats(pid) for pid in pids]
                    tick_ms = 1000 / os.sysconf("SC_CLK_TCK")
                    print(f"Concurrent loopback: {total} bytes + short echo {small_ms:.2f} ms; "
                          f"gateway/meow RSS pre={[x[0] for x in pre]} peak={peak} post={[x[0] for x in post]} KiB; "
                          f"CPU delta={[(b[1] - a[1]) * tick_ms for a,b in zip(pre,post)]} ms "
                          f"(resolution {tick_ms:.0f} ms)")
            print(f"OK: authenticated Wisp through real meow-rs, A/B/warm continuity and direct mode; "
                  f"three loopback controls {switch_a:.2f}/{switch_b:.2f}/{switch_direct:.2f} ms, "
                  f"fresh stream to mock upstream {fresh:.2f} ms")
        except Exception:
            print(f"meow test log:\n{log.read_text() if log.exists() else ''}", file=sys.stderr)
            raise
        finally:
            if meow_proc is not None:
                meow_proc.terminate()
                meow_proc.wait(timeout=4)
            child.terminate()
            child.wait(timeout=4)
            a.close()
            b.close()
            await a.wait_closed()
            await b.wait_closed()


if __name__ == "__main__":
    asyncio.run(smoke(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()))
