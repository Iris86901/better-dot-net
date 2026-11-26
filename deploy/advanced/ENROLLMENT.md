# Manage Advanced node identities

**English** · [简体中文](ENROLLMENT.zh-CN.md)

The collaboration daemon makes two checks. The **authentication directory**
maps a node ID to a credential digest and is loaded **on service startup**.
The **project membership** records which authenticated node IDs may use the
project and changes immediately when the owner grants or revokes access.
Adding a digest alone does not grant project access. The owner identity cannot
be removed with this tool.

## Add a node

Run on the VPS as the operator that owns the credential directory (root in the
reference deployment). The service must already have an initialized
`/etc/better-dot-net-advanced/nodes.json`. A separate per-node token and an ID
unique to that Agent make it possible to revoke one Agent without affecting
the others. Install the operator tool from a verified release into a
root-owned directory, such as `/opt/better-dot-net-advanced/sbin/enroll.py`.
Do not execute a user-writable checkout with sudo. If you override
`--directory`, use a trusted path whose parent directories are not writable
by untrusted users.

```sh
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py list
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py add build-agent-01
sudo systemctl restart bdn-collab.service
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 \
  /etc/better-dot-net-advanced/owner.token grant build-agent-01
```

The tool creates `node-build-agent-01.token` in the private operator directory,
mode 0600. It writes only the SHA-256 digest to `nodes.json` and never prints the
token. Give the token file only to its intended node using an approved private
delivery channel. Keep the node's copy mode 0600 and owned by the user running
its CLI. Do not pass tokens in command-line literals, URLs, chat or logs. The
reference loopback daemon is not reachable from another machine directly;
adding a node does not enable remote ingress.

Verify with the intended node's CLI using its token file and a known
collaboration endpoint. A 401 means the credential did not authenticate; 403
means the requested operation is denied. For a newly added node making the
standard `messages` call, 403 usually means the owner has not granted
membership yet.

## Revoke or rotate a node

For a lost token, first deny project access and then remove the digest. In-flight
work may already have received a response; these steps protect subsequent
requests.

```sh
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 \
  /etc/better-dot-net-advanced/owner.token revoke build-agent-01
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py remove build-agent-01
sudo systemctl restart bdn-collab.service
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py list
```

The removed ID is retired permanently by a private marker to prevent
re-enrollment with stale project membership. Enroll a **new** ID for a
replacement credential. Remove obsolete raw token copies after verifying the
old credential is rejected. Restoring historical authentication backups can
resurrect revoked digests: reconcile them against current retirement records
before restoring and restarting. A revoked or lost **owner** credential
requires a separate trusted recovery procedure.

A private token can remain if an add is interrupted before its digest is
committed. The tool never overwrites that file. Confirm the node is absent
with `list` and that no other operator is enrolling it before removing an
orphaned token and trying again. A failed removal does not guarantee digest
revocation: confirm `list`, complete the service restart, and verify rejection.
Use this tool for concurrent administrative operations; do not edit the JSON
manually while it runs.

## Test locally

```sh
python3 scripts/verify-enrollment.py
python3 scripts/verify-enrollment.py target/release/bdn-collab
```

The optional second test uses only a loopback listener and exercises the
actual daemon's authentication reload and project-access boundaries. No
production credentials or services are involved.
