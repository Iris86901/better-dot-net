# Vendored dependencies

`oauth-as/` is the full published Cargo source for oauth-as 1.1.0,
upstream revision `006b9df72c804f7356e0bf1e7cd8f880cee9f716`
(https://github.com/MattJackson/oauth-as).
The original MIT and Apache-2.0 notices and the published tests are retained.
Do not edit the vendored code except for explicitly documented adapter patches.

Source archive SHA-256:
`e8704847e09c0cf9f33526b2840f18bb5f395c966a461a1db886ed3ec1c2f2a8`.

BDN adds one opt-in feature, `bdn-snapshot`, to round-trip upstream's
reference store (including consumed grants and revocation barriers) into a
private, atomically replaced local file, and to revoke a single
`(client_id, subject)` pair via upstream's existing `RevocationBarrier::Consent`
without disturbing other subjects sharing a CIMD client. No protocol logic is
patched. The app serializes control writes and only acknowledges a successful
grant after the snapshot is saved. Do not share the file among daemon processes.
