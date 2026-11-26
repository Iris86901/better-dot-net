# Advanced terminal: named local streams

This opt-in Rust program connects a terminal or local process to an
operator-named TCP service. Both the relay and the service target are pinned to
numeric loopback addresses. There is no SOCKS, HTTP CONNECT, dynamic hostname
resolution, arbitrary callback, shell execution, or listener on an exposed
interface. The Basic browser gateway remains separate. This testable slice
does not provide a general Internet proxy or demonstrate cross-machine relay.

## Local acceptance

```sh
cargo build --locked --features terminal-cli --bin bdn-terminal
python3 scripts/verify-terminal.py target/debug/bdn-terminal
```

The terminal-cli feature enables this binary. The Basic runtime does not start it.
The fixture generates private credentials and a temporary TLS certificate.
It tests authorized 1 MiB binary transfer with correct half-close behavior,
four simultaneous clients, TLS verification failure, unavailable target
handling and same-target recovery, and denial of unknown services, mis-scoped credentials, and
non-loopback routing before any target connection.

## Operator contract

```
bdn-terminal serve 127.0.0.1:9400 SERVER_CERT_PEM SERVER_KEY_PEM POLICY_JSON
bdn-terminal connect 127.0.0.1:9400 TERMINAL_DNS_NAME CA_PEM SERVICE TOKEN_FILE
```

The DNS name is for certificate verification only; it is not resolved. The
client sends the secret over authenticated TLS 1.3 with an explicit protocol
identifier. Standard input/output are binary; status goes to standard error.
Local input EOF half-closes the connection; target EOF terminates the client.
Do not pipe a generic shell or an untrusted local service to this program.

The operator supplies an explicit CA, the gateway certificate and private key,
a policy, and separate randomly generated 32-byte tokens per grant. Keep the
policy, private key, and token files owner-only, in trusted directories. The
token file holds one 64-digit hex token and an optional trailing newline. A
policy contains up to 32 named routes with 1–16 unique SHA-256 digests per
route. Each digest is computed on the **32 decoded bytes**, rather than on
the hexadecimal text. A digest is authorized for exactly one route.

```json
[
  {
    "name": "build-data",
    "target": "127.0.0.1:9411",
    "grant_sha256": ["<64 hex digits for each authorized token>"]
  }
]
```

The process accepts no public bind or target, keeps at most 16 active
connections, and limits handshake and session duration. Restart with a
modified policy to revoke a grant; restart closes existing streams.
Use a trusted host or network namespace where unauthorized local processes
cannot replace the configured service by taking over its listening port.
Do not expose the relay through a general-purpose reverse proxy.

## External and reverse service acceptance gates

Before an explicitly authorized cross-machine trial, identify the sanctioned
connection path and the named services the operator owns. Preserve the local
gateway contract; run tested private transport separately from the Basic
gateway. Require distinct node identities, per-service read/use grants and
a deny-by-default endpoint inventory. Use an external client with its own
credential and demonstrate receipt of synthetic bytes from a fixed,
owner-controlled service. A successful local fixture is not evidence of an
allowed external route.

For reverse communication, the operator must first choose a particular
owned service and its exact local port. Keep a versioned, finite service
inventory and bind the publisher identity to the named route. A receiver may
ask for that name only, never provide an IP, port, command, or protocol.
Publication and consumption authorization are separate; an agent task or
message does not grant either. Use authenticated allowlisted identities,
simultaneous-stream caps, handshake and session deadlines, and revocation.
A disconnect must withdraw the name and reject new streams until the same
registered publisher reconnects; it must not redirect to an incidental
service. Test identity separation, expired grant denial, disconnect and
reconnect, two publishers, and the unchanged Basic gateway before production
activation. Reverse publication is not implemented by this local slice.
