"""Exercise one-time invitation redemption and Release-client local join layout."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode

ORIGIN = "https://collab.example.test"


def verify(server, cli):
    with tempfile.TemporaryDirectory(prefix="bdn-join-recovery-") as tmp:
        root = Path(tmp)
        keys = {name: secrets.token_hex(32) for name in ("owner", "agent_a", "agent_b", "outsider")}
        directory = root / "nodes.json"
        directory.touch(mode=0o600)
        directory.write_text(json.dumps({"owner": "owner", "nodes": {
            name: hashlib.sha256(token.encode()).hexdigest() for name, token in keys.items()
        }}))
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        addr = f"127.0.0.1:{port}"
        invites = root / "invites"
        invites.mkdir(mode=0o700)
        env = {**os.environ, "BDN_INVITE_DIR": str(invites), "BDN_RECOVERY_ORIGIN": ORIGIN}

        def call(method, path, data=None, **headers):
            link = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
            try:
                link.request(method, path, data, headers)
                res = link.getresponse()
                return res.status, dict((k.lower(), v) for k, v in res.getheaders()), res.read()
            finally:
                link.close()

        def member_action(action, node):
            code, _, _ = call("POST", "/v1/action", json.dumps({"action": action, "node": node}),
                              Authorization="Bearer " + keys["owner"])
            assert code == 200

        proc = subprocess.Popen([server, str(directory), str(root / "state"), addr],
                                env=env, stdout=subprocess.DEVNULL)
        try:
            for _ in range(30):
                try:
                    if call("GET", "/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError("fixture didn't start")

            def make_invite(node, duration=120):
                code = secrets.token_hex(32)
                path = invites / hashlib.sha256(code.encode()).hexdigest()
                path.write_text(f"{int(time.time()) + duration}\n{keys[node]}\n")
                return code

            def redeem_json(code):
                return call("POST", "/agent/invite", json.dumps({"code": code}),
                            **{"Content-Type": "application/json"})

            def redeem_form(code, origin=ORIGIN):
                return call("POST", "/agent/invite", urlencode({"code": code}),
                            Origin=origin, **{"Content-Type": "application/x-www-form-urlencoded"})

            # Browser binary hosting and download cookies are removed; clients come from Release.
            assert call("GET", "/agent/client/x86_64")[0] == 404
            base = call("GET", "/agent")
            assert base[0] == 200 and "接受邀请".encode() in base[2]
            assert base[1]["referrer-policy"] == "same-origin"
            assert "set-cookie" not in base[1]

            # Expired, owner, and ungranted outsider invites are rejected.
            assert redeem_json(make_invite("agent_a", -1))[0] == 403
            assert redeem_json(make_invite("owner"))[0] == 403
            assert redeem_json(make_invite("outsider"))[0] == 403

            member_action("grant", "agent_a")
            member_action("grant", "agent_b")

            same = make_invite("agent_a")
            different = make_invite("agent_a")
            assert redeem_form(same, "https://wrong.example.test")[0] == 403
            assert redeem_form(same, "null")[0] == 403

            # 8-way concurrent race across same + second invite yields strictly one 200.
            with ThreadPoolExecutor(max_workers=8) as pool:
                outcomes = list(pool.map(redeem_json, [same] * 7 + [different]))
            assert sorted(row[0] for row in outcomes) == [200] + [403] * 7
            _, headers, first = next(row for row in outcomes if row[0] == 200)
            assert headers["cache-control"] == "no-store"
            assert headers["referrer-policy"] == "no-referrer"
            assert "set-cookie" not in headers
            payload = json.loads(first)
            assert payload == {"node": "agent_a", "credential": keys["agent_a"]}

            # Replays and new invites for an already-claimed node are rejected.
            assert redeem_json(same)[0] == 403
            assert redeem_json(make_invite("agent_a"))[0] == 403

            # Simulate Release client placed in its own 0700 directory (e.g. /workspace/shared/better-dot-net/bdn)
            # with BDN_HOME unset so config.json + credentials live right beside bdn.
            dot_dir = root / "better-dot-net"
            dot_dir.mkdir(mode=0o700)
            dot_bin = dot_dir / "bdn"
            shutil.copy2(cli, dot_bin)
            dot_bin.chmod(0o700)
            clean_env = {k: v for k, v in os.environ.items() if k != "BDN_HOME"}

            invite_b = make_invite("agent_b")
            joined = subprocess.run(
                [str(dot_bin), "join", addr],
                input=invite_b + "\n",
                text=True,
                env=clean_env,
                check=True,
                capture_output=True,
                timeout=8,
            )
            # Never leak credential to stdout.
            assert keys["agent_b"] not in joined.stdout
            assert json.loads(joined.stdout) == {"ok": True, "node": "agent_b", "origin": addr}

            cfg_path = dot_dir / "config.json"
            cred_path = dot_dir / "credentials"
            assert cfg_path.is_file() and cred_path.is_file()
            assert cred_path.stat().st_mode & 0o777 == 0o600
            assert cfg_path.stat().st_mode & 0o777 == 0o600
            cfg = json.loads(cfg_path.read_text())
            assert cfg == {"origin": addr, "node": "agent_b"}
            assert cred_path.read_text().strip() == keys["agent_b"]

            # Subsequent commands work with zero arguments or env vars, reading beside bdn.
            post_res = subprocess.run(
                [str(dot_bin), "post", "after-join", "saved"],
                env=clean_env,
                check=True,
                capture_output=True,
                timeout=8,
            )
            assert json.loads(post_res.stdout)["seq"] == 1

            # Second join refuses to overwrite existing credentials.
            second_join = subprocess.run(
                [str(dot_bin), "join", addr, secrets.token_hex(32)],
                env=clean_env,
                capture_output=True,
                timeout=8,
            )
            assert second_join.returncode != 0

            # World-readable credentials file is rejected.
            cred_path.chmod(0o644)
            assert subprocess.run(
                [str(dot_bin), "tasks"],
                env=clean_env,
                capture_output=True,
                timeout=8,
            ).returncode != 0
            cred_path.chmod(0o600)

            # Revoking agent_b immediately blocks saved client commands.
            member_action("revoke", "agent_b")
            revoked = subprocess.run(
                [str(dot_bin), "tasks"],
                env=clean_env,
                capture_output=True,
                timeout=8,
            )
            assert revoked.returncode != 0 and b"HTTP 403" in revoked.stderr
        finally:
            proc.terminate()
            proc.wait(timeout=5)
    print("PASS: once-per-identity JSON join, Release-dir config.json + 0600 credentials, no binary hosting, and revocation")


if __name__ == "__main__":
    verify(*(str(Path(p).resolve()) for p in sys.argv[1:]))
