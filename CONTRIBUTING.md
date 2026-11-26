# Contributing to Better Dot Net

[简体中文](CONTRIBUTING.zh-CN.md)

Contributions are welcome, including bug reports, documentation improvements, focused fixes, and new capabilities that fit Better Dot Net's architecture (low-footprint Rust core, decoupled Basic / Advanced / Client modules).

Contributions created with coding agents are also welcome. Regardless of tooling, the contributor remains responsible for reviewing the resulting diff, validating the change, and ensuring no credentials, personal domains, public VPS IPs, or machine-private data are included.

## Before you start

- Check the latest `main` and existing issues before starting substantial work.
- Small documentation fixes and focused bug fixes can be submitted directly. For larger features, protocol changes, or architectural changes, open an issue first to align on design and avoid duplicated effort.
- Keep Basic Server (`better-dot-net`), Advanced Server (`bdn-collab` / `bdn-terminal`), and the static Agent Client (`bdn`) decoupled. Upgrading any module must preserve existing credentials and connectivity.
- Keep changes focused. Avoid unrelated refactors, formatting churn, or generated artifacts in the same pull request.
- Never include real access keys, invite codes, Bearer tokens, cookies, private keys, personal domains, real VPS IPs, or image EXIF metadata in issues, PRs, logs, or screenshots.

## Reporting issues

A useful issue should let a maintainer understand the observed problem without first asking for basic context. Include what is relevant; there is no need to fill unrelated fields with `N/A`.

For bugs and site/environment compatibility issues, include when available:

- **Observed behavior and impact:** which module (`Basic`, `Advanced`, or `bdn`), action, website, or workflow failed, what actually happened, and whether it blocks normal usage.
- **Environment:** Better Dot Net release tag or commit, VPS OS/architecture, reverse proxy (`Caddy` / `Nginx`), and browser or Agent platform (`Dot`, `Muse`, `Hark`, local Chrome/Safari, etc.).
- **Minimal reproduction:** shortest steps to trigger the issue, and whether it is deterministic or intermittent.
- **Expected versus actual behavior:** state them separately.
- **Exact evidence:** preserve exact error text, HTTP status codes, console errors, or a short redacted log tail.
- **Facts versus assessment:** distinguish directly observed facts from a suspected root cause.

### Issues written by coding agents

Coding agents are welcome to open issues, and should include low-cost diagnostics already available to them (exact error output, inspected commit/files, focused test evidence) while:

- stating whether the problem was **reproduced directly**, **inferred from source/tests**, or **reported without independent reproduction**;
- never inventing missing environment details, reproduction results, or root causes;
- labeling any unproven root cause as **suspected**.

## Development workflow

1. Start from the current `main` branch and create a focused branch (e.g. `fix/...`, `feat/...`, `docs/...`).
2. Match the existing architecture and naming. Prefer the smallest coherent change that solves the problem cleanly.
3. Add or update focused tests when behavior changes, and update paired English / Chinese docs together when public behavior or deployment steps change.
4. Run the smallest relevant validation before opening a PR:
   - **Web / JS changes:**
     ```bash
     node --check web/app.js && node --check web/sw.js
     node scripts/verify-reconnect.mjs
     ```
   - **Rust Basic changes:**
     ```bash
     cargo fmt --check
     cargo test --locked --bin better-dot-net
     ```
   - **Rust Advanced / CLI changes:**
     ```bash
     cargo fmt --check
     cargo test --locked --features advanced,collab-cli,terminal-cli
     ```
   - **Documentation-only changes:** run `git diff --check` and verify links, anchors, and commands against their sources; no Cargo build is required.
5. Review the final diff (`git status`, `git diff`) to verify there are no unrelated changes, temporary files, credentials, or private domains/IPs.

## Pull requests

Each pull request should stay reviewable and solve **one coherent purpose**. In the PR description, clearly state:

- **What changed and why** (link related issue(s) when applicable).
- **Validation performed** (exact commands run and observed results, or manual browser/VPS verification steps).
- **Trust-boundary regression evidence:** if the change touches authentication, cookies, SSRF/DNS policy, Clash YAML sanitization, tunnel/reverse-expose access control, enrollment/revocation, or persistence, include focused tests or concrete verification for that boundary.
- **Privacy & hygiene check:** confirm no secrets, real domains/IPs, or personal metadata are present in code, docs, or assets.
