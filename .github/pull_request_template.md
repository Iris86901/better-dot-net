## Summary

<!-- Briefly describe what changed and why. Link related issues (e.g. Fixes #123). -->

## Scope

- [ ] Basic Server (`better-dot-net`)
- [ ] Basic Web Proxy Frontend (`web/`)
- [ ] Advanced Server / CLI (`bdn-collab`, `bdn`, `bdn-terminal`)
- [ ] Deployment / Documentation

## Validation

<!-- List the exact commands and/or live browser/VPS checks you ran and their results. -->

- [ ] `node --check web/app.js && node --check web/sw.js && node scripts/verify-reconnect.mjs` (if `web/` changed)
- [ ] `cargo fmt --check` and relevant `cargo test` (if Rust changed)
- [ ] `git diff --check` (for docs/config)

## Trust Boundary & Privacy Checklist

- [ ] Focused diff with one coherent purpose (no unrelated refactors or generated junk)
- [ ] If auth, cookies, SSRF/DNS policy, Clash sanitization, tunnel/expose access control, or persistence changed, regression evidence is included above
- [ ] Confirmed zero real access keys, invite codes, tokens, cookies, private keys, personal domains/IPs, or image EXIF metadata
- [ ] Paired English / Chinese docs updated where public behavior or deployment steps changed
