#!/usr/bin/env python3
"""Isolated fixture; opens only ephemeral host loopback sockets."""
import concurrent.futures
import contextlib
import hashlib
import json
import os
import pathlib
import secrets
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading

TIMEOUT = 10


class Echo(socketserver.ThreadingTCPServer):
    daemon_threads = True
    requests = 0
    counter_lock = threading.Lock()

    def process_request(self, request, addr):
        with self.counter_lock:
            self.requests += 1
        super().process_request(request, addr)


class HashEcho(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(TIMEOUT)
        # Send only after inbound EOF to exercise the bidirectional half-close.
        digest = hashlib.sha256()
        count = 0
        while data := self.request.recv(8192):
            digest.update(data)
            count += len(data)
        self.request.sendall(f"{count}:{digest.hexdigest()}".encode())


@contextlib.contextmanager
def target():
    with Echo(("127.0.0.1", 0), HashEcho) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield server
        finally:
            server.shutdown()
            worker.join(timeout=TIMEOUT)


def address(port):
    return f"127.0.0.1:{port}"


def port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reserved:
        reserved.bind(("127.0.0.1", 0))
        return reserved.getsockname()[1]


def private(path, data):
    with os.fdopen(
        os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w"
    ) as output:
        output.write(data)
    return str(path)


def main(binary):
    binary = str(pathlib.Path(binary).resolve())
    with tempfile.TemporaryDirectory(prefix="bdn-terminal-") as dirname, target() as a, target() as b:
        directory = pathlib.Path(dirname)
        cert, key = directory / "cert.pem", directory / "issuer.key"
        subprocess.run(
            [
                "openssl", "req", "-x509", "-nodes", "-newkey", "ec",
                "-pkeyopt", "ec_paramgen_curve:P-256", "-days", "1",
                "-subj", "/CN=terminal-fixture.test",
                "-addext", "subjectAltName=DNS:terminal-fixture.test",
                "-addext", "basicConstraints=critical,CA:FALSE",
                "-out", cert, "-keyout", key,
            ],
            check=True, capture_output=True, timeout=TIMEOUT,
        )
        key.chmod(0o600)
        tokens = [secrets.token_bytes(32) for _ in range(3)]
        files = [private(directory / f"token-{i}", token.hex() + "\n") for i, token in enumerate(tokens)]
        relay = address(port())
        routes = [
            {"name": name, "target": address(server.server_address[1]),
             "grant_sha256": [hashlib.sha256(token).hexdigest()]}
            for name, server, token in zip(("alpha", "beta"), (a, b), tokens)
        ]
        with socket.socket() as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            routes.append({"name": "offline", "target": address(unavailable.getsockname()[1]),
                           "grant_sha256": [hashlib.sha256(tokens[2]).hexdigest()]})
            policy = private(directory / "policy.json", json.dumps(routes))
            args = [binary, "serve", relay, str(cert), str(key), policy]

            external = routes + [{"name": "not-local", "target": "192.0.2.1:80", "grant_sha256": ["12" * 32]}]
            blocked = private(directory / "blocked.json", json.dumps(external))
            reject = subprocess.run([*args[:-1], blocked], capture_output=True, timeout=TIMEOUT)
            assert reject.returncode and b"127.0.0.1 or ::1" in reject.stderr
            duplicate = [*routes, {**routes[0], "name": "duplicate"}]
            duplicate_policy = private(directory / "duplicate.json", json.dumps(duplicate))
            reject = subprocess.run(
                [*args[:-1], duplicate_policy], capture_output=True, timeout=TIMEOUT
            )
            assert reject.returncode and b"duplicate terminal credential" in reject.stderr
            public_policy = directory / "public.json"
            public_policy.write_text(json.dumps(routes))
            public_policy.chmod(0o644)
            reject = subprocess.run(
                [*args[:-1], public_policy], capture_output=True, timeout=TIMEOUT
            )
            assert reject.returncode and b"file must be private" in reject.stderr
            deny_listen = subprocess.run(
                [*args[:2], "0.0.0.0:8080", *args[3:]], capture_output=True, timeout=TIMEOUT
            )
            assert deny_listen.returncode and b"127.0.0.1 or ::1" in deny_listen.stderr

            relay_process = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                import select
                assert select.select([relay_process.stderr], [], [], TIMEOUT)[0], "relay did not start"
                assert relay_process.stderr.readline().strip() == b"terminal relay ready"

                def connect(service, token=0, payload=b"", name="terminal-fixture.test", host=relay):
                    return subprocess.run(
                        [binary, "connect", host, name, cert, service, files[token]],
                        input=payload, capture_output=True, timeout=TIMEOUT,
                    )

                for service, token in [("unknown", 0), ("beta", 0), ("alpha", 1)]:
                    denied = connect(service, token)
                    assert denied.returncode and b"terminal access denied" in denied.stderr, denied
                    assert not denied.stdout
                wrong_cert = connect("alpha", name="other-fixture.test")
                assert wrong_cert.returncode and b"terminal TLS verification failed" in wrong_cert.stderr
                nonlocal_client = connect("alpha", host="192.0.2.1:443")
                assert nonlocal_client.returncode and b"127.0.0.1 or ::1" in nonlocal_client.stderr
                offline = connect("offline", 2)
                assert offline.returncode and b"configured terminal service unavailable" in offline.stderr
                pathlib.Path(files[0]).chmod(0o644)
                reject = connect("alpha")
                assert reject.returncode and b"file must be private" in reject.stderr
                pathlib.Path(files[0]).chmod(0o600)
                assert (a.requests, b.requests) == (0, 0), "unauthorized request dialed a target"

                def check(service, token, payload):
                    result = connect(service, token, payload)
                    assert not result.returncode, result.stderr
                    assert result.stdout == f"{len(payload)}:{hashlib.sha256(payload).hexdigest()}".encode()

                check("alpha", 0, bytes(range(256)) * 4096)
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                    futures = [
                        executor.submit(check, service, number, bytes([byte]) * 100_000)
                        for service, number in [("alpha", 0), ("beta", 1)] for byte in range(2)
                    ]
                    for future in futures:
                        future.result()
                assert (a.requests, b.requests) == (3, 2)

                # The exact named address becomes available again, without redirecting
                # to another service or restarting the relay. The target closes its
                # write side first; the CLI must exit even with stdin still open.
                unavailable.listen(1)
                unavailable.settimeout(TIMEOUT)

                def recovered_service():
                    peer, _ = unavailable.accept()
                    with peer:
                        peer.sendall(b"configured service back")
                        peer.shutdown(socket.SHUT_WR)

                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                    recovered = executor.submit(recovered_service)
                    with subprocess.Popen(
                        [binary, "connect", relay, "terminal-fixture.test", cert, "offline", files[2]],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    ) as client:
                        try:
                            assert client.wait(timeout=TIMEOUT) == 0
                            assert client.stdout.read() == b"configured service back"
                        finally:
                            if client.poll() is None:
                                client.kill()
                    recovered.result(timeout=TIMEOUT)
                assert (a.requests, b.requests) == (3, 2)
            finally:
                relay_process.terminate()
                try:
                    relay_process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    relay_process.kill()
                    relay_process.wait(timeout=3)
    print("PASS: private TLS named streams, exact half-close transfer, 4 concurrent clients, denied routing before connect, TLS failure, target recovery, no non-loopback addresses")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} PATH_TO_BDN_TERMINAL")
    main(sys.argv[1])
