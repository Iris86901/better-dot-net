"""Exercise real TLS verification and heartbeat on loopback only."""
import asyncio
import os
import ssl
import subprocess
import sys
import tempfile
from pathlib import Path
from websockets.asyncio.server import serve

async def main(binary):
    with tempfile.TemporaryDirectory() as d:
        p = Path(d)
        def openssl(*args):
            subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, cwd=p)
        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                "-subj", "/CN=Loopback test CA", "-keyout", "ca.key", "-out", "ca.pem")
        openssl("req", "-new", "-newkey", "rsa:2048", "-nodes",
                "-subj", "/CN=Loopback fixture", "-keyout", "server.key", "-out", "server.csr")
        (p / "ext.cnf").write_text("subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n")
        openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
                "-CAcreateserial", "-days", "1", "-extfile", "ext.cnf", "-out", "server.pem")
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(p/"server.pem", p/"server.key")
        attempts = 0
        async def fixture(ws):
            nonlocal attempts
            attempts += 1
            assert ws.request.headers.get("Cookie") == "fixture=sent-via-stdin"
            await ws.send(b"unused test greeting")
            await ws.wait_closed()
        async def command(host, trusted):
            env = {**os.environ}
            env.pop("SSL_CERT_FILE", None)
            if trusted:
                env["SSL_CERT_FILE"] = str(p/"ca.pem")
            proc = await asyncio.create_subprocess_exec(
                binary, f"wss://{host}:{port}/check", "1", "--cookie-stdin", env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            out, err = await asyncio.wait_for(proc.communicate(b"fixture=sent-via-stdin\n"), 15)
            assert b"fixture=sent-via-stdin" not in out + err
            return proc.returncode, out, err
        async with serve(fixture, "127.0.0.1", 0, ssl=ctx) as listener:
            port = listener.sockets[0].getsockname()[1]
            bad_trust = await command("127.0.0.1", False)
            bad_name = await command("localhost", True)
            ok = await command("127.0.0.1", True)
            assert bad_trust[0] and b"UnknownIssuer" in bad_trust[2], bad_trust
            assert bad_name[0] and b"certificate not valid for name" in bad_name[2], bad_name
            assert ok[0] == 0 and ok[1].count(b"PONG") == 2 and b"PASS_DURATION" in ok[1], ok
            assert attempts == 1, attempts
            print("PASS: IP SAN verified, wrong name and untrusted certificate rejected, two Pongs on one WSS")
if __name__ == "__main__":
    asyncio.run(main(str(Path(sys.argv[1]).resolve())))
