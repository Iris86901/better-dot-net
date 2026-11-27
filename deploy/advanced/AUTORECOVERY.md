# Optional Dot automatic recovery (experimental, Advanced only)

The Advanced daemon continues to work unchanged when automatic recovery is off.
It is off by default and shares neither a public hostname nor state files with
Basic. The feature uses the existing authenticated WSS tunnel (`ExposeHub`) as
its direct, in-process observation source — no external probe host or relay is
needed. A live tunnel, an accepted event, a startup report, and a healthy
application are separate observations.

## Enable and pair

1. In a separate Advanced instance first, configure the exact HTTPS control
   origin in `BDN_AUTORECOVERY_ORIGIN` (no path or trailing slash), the exact
   verified webhook hostname in `BDN_RECOVERY_CALLBACK_DOMAIN`, and enable
   the existing Advanced tunnel. If ChatGPT shows a callback-specific redirect,
   set `BDN_RECOVERY_REDIRECT_URI` to that exact HTTPS URL; the default is the
   stable `https://chatgpt.com/connector_platform_oauth_redirect`. Enable only
   the optional Advanced control routes in the example Caddyfile.
2. On an already enrolled, non-owner Dot node, run `bdn recovery enable` in a
   private terminal. It uses the node's existing long-term `bdn` credential to
   request a 10-minute one-use pairing code (plus a fallback static OAuth
   Client ID/Secret) and never displays or persists the original node
   credential. Do not paste secrets or pairing codes into logs, issue trackers,
   shared files, conversation transcripts or command arguments.
   - **Recommended (CIMD)**: In ChatGPT's custom MCP OAuth configuration, enter
     only the MCP URL, leave Client ID and Client Secret blank so ChatGPT uses
     its Client ID Metadata Document (`https://chatgpt.com/oauth/client.json`)
     with PKCE S256, then enter the pairing code on the trusted HTTPS
     authorization page.
   - **Fallback (Static Client)**: If using a callback-specific redirect URI,
     enter the returned Client ID and Secret along with the MCP URL, then enter
     the same pairing code on the authorization page.
   Dynamic Client Registration (`/oauth/register`) is intentionally disabled.
   Re-running `enable` before pairing succeeds replaces and cleans up the
   previous pending static client.
3. Confirm `get_recovery_status` (or `bdn recovery status` from the node)
   reports the correct node and `eligibility_reason`. In Dot, authorize a
   recovery task to subscribe to `bdn.recovery_requested` filtered to that
   single node. Pre-approve only a fixed, locally reviewed startup entry;
   event data contains IDs, not an executable command.

## Recovery task contract

Use the event's node ID and incident ID as data. Query `get_recovery_status`
first. If the node is online, paused, mismatched, or `claimable` is false, stop
and inspect `eligibility_reason`. Only a successful `claim_recovery` for the
current incident allows the pre-authorized local startup entry to run. A
duplicate callback, reminder event, or unknown result is not permission to run
twice. Run one fixed idempotent local startup entry, then report via
`report_recovery` using that incident and lease. Confirm a fresh tunnel and
actual application response separately. HTTP 2xx from the event receiver means
only that it received the event.

For planned downtime, run `bdn recovery pause` **before** stopping the tunnel or
application. Run `bdn recovery resume` afterward; this keeps OAuth intact.
`bdn recovery disable` clears its authorization, revokes its OAuth tokens
(without disturbing other nodes sharing ChatGPT's CIMD client), and removes its
subscription; the normal WSS and node key are unaffected. Project node
revocation removes recovery rights and OAuth tokens too. If the platform lost
the installation or credentials, stop and use the existing enrollment/recovery
procedure; do not re-enroll silently.

## Defaults and maintenance

- Five-second in-process tunnel observation (backed by 10s/30s tunnel keepalive);
  45 seconds sustained disconnect after a previously observed connection. At most
  one incident per observed outage.
- Transport retries: up to 3 attempts per event ID with increasing backoff
  (30s, 60s, 120s), keeping the same `eventId`. HTTP `410`/`413` stops retries
  permanently.
- Unclaimed incident lifetime & bounded follow-up reminders: an unclaimed
  incident remains claimable for as long as the node stays offline (accommodating
  ChatGPT asynchronous batching delays). After the initial webhook receives HTTP
  2xx, if the incident remains unclaimed while the node is still offline, up to
  2 reminder events (with new `eventId`s pointing to the same `incident_id`) are
  sent every 30 minutes within a 2-hour window. Reminders never grant extra
  claims.
- Claim policy: 1 claim per incident, 1-hour cooldown, and at most 3 claims per
  24-hour window. Post-claim execution/report lease TTL is 15 minutes.
- OAuth grants: scoped to the `/mcp` resource and that single node. Refresh
  tokens rotate with reuse detection and have a **30-day absolute expiry** from
  initial pairing (not extended on refresh). Expired OAuth records are swept
  automatically in the background. When access expires or is revoked, run
  `bdn recovery disable` and pair again.
- State is in the Advanced data directory in `autorecovery-v1.json` (0600) and
  `autorecovery-oauth-v1.json` (0600), separate from node credentials and the
  invitation system. Protect backups: `autorecovery-oauth-v1.json` contains live
  OAuth bearer material, and `autorecovery-v1.json` contains webhook signing
  keys. Use a unique directory for each daemon; never run multiple daemons
  against one state directory.

References: [OpenAI OAuth](https://developers.openai.com/plugins/build/auth),
[OpenAI MCP Events](https://developers.openai.com/plugins/build/mcp-events),
[dots-tailscale recovery evidence](https://github.com/abruption/dots-tailscale/issues/43).
