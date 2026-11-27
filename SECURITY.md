# Security policy

Report a vulnerability privately: open this repository's **Security** tab and
choose **Report a vulnerability**. Do not open a public issue or pull request
for a vulnerability.

Include the affected release tag or commit, the steps to reproduce, and what
an attacker gains. A report gets a reply in the advisory thread, and the fix
is published with the advisory.

## Supported versions

Fixes go to `main` and the next tagged daemon release. Earlier prereleases
are not updated.

## Scope

This repository covers `tsrsd`: its HTTP proxy, LocalAPI socket, CLI shim,
key and state files, and the changes in `tailscale-rs.patch`.
Vulnerabilities in upstream tailscale-rs or tokio code belong with those
projects unless the Q2 changes introduced them.
