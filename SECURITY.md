# Security policy

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report them privately through
GitHub: **Security → Report a vulnerability** on this repository. Include what you found, how to
reproduce it, and which version or commit you used.

You'll get a reply within a week. Once a fix is released, the advisory is published with credit
to you, unless you'd rather stay anonymous.

## Supported versions

cubeharbor is pre-1.0. Only the latest release gets security fixes.

## Deployment notes

See [Security notes](README.md#security-notes) in the README. In short: put the dashboard behind
TLS, keep its password secret (it has cluster-wide rights on `MinecraftServer` resources and
reads RCON secrets), and only enable `dashboard.publicServerList` if you want the list public.
