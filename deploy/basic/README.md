# Better Dot Net: Basic deployment

**English** · [简体中文](README.zh-CN.md)

Basic bundles the gateway, the browser runtime, and the pinned meow-rs
routing core and license. It starts the core even when the selected route is
direct VPS egress. Users can switch to a configured upstream without
installing Agent software. Advanced is independent and not installed.

## Build and install

- Build the x86-64 Linux gateway (glibc 2.34 or newer on the target).
  Run `deploy/basic/package.sh GATEWAY NEW_DIRECTORY MEOW_RELEASE_ARCHIVE`.
  It checks the pinned meow-rs v0.29.0 asset before extracting the binary.
- Verify the archive checksum and extracted `basic/SHA256SUMS`. Copy the
  complete tree to a root-owned, read-only release directory under
  `/opt/better-dot-net-basic/releases/`. Public assets and their parent
  directories must be accessible to Caddy. Point `current` at the release.
- Install `better-dot-net-basic.sysusers` as
  `/etc/sysusers.d/better-dot-net-basic.conf` and use `systemd-sysusers` to
  create the dedicated account shared by both Basic services.
- Create `/etc/better-dot-net-basic/environment` (root, mode 0600) with
  independent random `ACCESS_KEY`, `COOKIE_SECRET`, and `MEOW_SECRET`.
  Keep them out of public URLs and logs. Leave `UPSTREAM_PROXY` and
  `UPSTREAM_SOCKS5` unset for a fresh direct-mode setup. The core control and
  proxy ports stay on loopback (defaults 19190 and 17990).
- Install both unit files and run `systemctl daemon-reload`. Merge the sample
  Caddy site, adjust gateway port 3030 if needed, and validate before applying.
  Start with `systemctl enable --now better-dot-net-basic.service`. It starts
  the core and verifies its authenticated controller before serving.
- Verify gateway health, browser authentication, a rendered test page and
  reload. Verify a configured upstream actually carries the target traffic.

## Browser routing panel

Choose **Direct VPS**, **Pasted Nodes**, or **Remote Clash**. Pasted Nodes
accepts one URI or shorthand per line (`host:port`,
`host:port:user:password`, or an explicit `socks5://` URI for SOCKS).
The two profiles keep separate node lists and selections. Selecting a node
changes new connections; replacing a profile may interrupt existing streams
through that profile.

Remote Clash **Import** is enabled by default for authenticated users on public
HTTPS (port 443) subscription hostnames, validating the certificate, blocking
private/loopback subscription hosts, proxy endpoints, and redirects, and
validating the inline `proxies:` list before saving. Operators can restrict
imports to a comma-separated allowlist of exact DNS hostnames via
`BDN_SUBSCRIPTION_HOSTS`, or set `BDN_SUBSCRIPTION_HOSTS=none` to disable
remote URL imports. It leaves the current route unchanged unless Remote Clash
is already live. Re-importing keeps the current selection if it is still
present; if that node disappeared from a live Remote Clash profile, the
first replacement node takes over. **Use Selected Node** switches routes;
**Delete** removes the saved profile and falls back to Direct only if that
profile was active. Failed imports leave the previous profile intact. The
URL and its query tokens are never stored or returned.

## Access by literal IP

The browser entry can use `https://IP[:PORT]/` without DNS. The edge needs a
browser-trusted certificate with the IP in its subjectAltName and must select
that certificate for an IP client without SNI. For an Internet deployment
ensure the address reaches the listener and automate certificate renewal.
Public plaintext HTTP cannot provide the secure context needed by Scramjet's
service worker; loopback HTTP is a development exception. An IP is a different
browser origin from a DNS name and has its own login, cookies and tab state.

## Upgrade and rollback

Verify the new files before atomically switching `current`; keep the previous
release, private state and cookie secret. Restart both Basic units together:
`systemctl restart better-dot-net-basic-core.service better-dot-net-basic.service`.
For rollback restore the old symlink and repeat. For releases predating the
bundled core, restore their unit definitions too. Verify browser operation
before deleting the prior release; leave Advanced and unrelated sites alone.
