# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Added

* Optional Cloudflare DNS updates for home installs with a changing public IP
  (`cloudflareDdns.enabled`, off by default). The operator looks up its public IPv4 every
  `interval` (5m). When it changed, and once an hour to repair edits made in Cloudflare, it
  creates or updates the A records (`*.<baseDomain>` by default, `records` to choose,
  `serverHostnames` for servers' own hostnames in the zone), always as DNS only (not proxied). Other records are left alone. The API token comes from an existing
  Secret, errors are retried with backoff, and no new RBAC is needed. IPv4 only. See "DNS setup"
  in the README.
* Dashboard: an **Account** page to change the login password (current password required,
  at least 12 characters). It's saved to the dashboard's Secret and logs out other sessions
  (`dashboard.allowPasswordChange`, on by default; off with `existingSecret`). The README now
  explains choosing your own password at install.

### Changed

* CRD: the API server now rejects MinecraftServers whose name isn't a DNS-1035 label of at most
  52 characters (such servers could never get a Service). An existing server with such a name
  can no longer be updated once the new CRD is applied: delete it and recreate it under a valid
  name (its world volume `data-<name>-0` is kept and can be copied over). Re-apply the chart's CRD (`kubectl apply -f charts/cubeharbor/crds/`) when upgrading:
  Helm does not update CRDs.

### Fixed

* Dashboard: **Delete with data** now reports an error when the world volume
  (`data-<name>-0`) can't be deleted, instead of redirecting as if it was. A volume that's
  already gone is still fine, and the message includes the `kubectl` command to delete it.
* Servers only count as ready (`Running`) once they answer a status ping. Vanilla opens its port
  before the world is loaded, so a server used to show `Running`, and the gateway forwarded
  players to it, while it was still preparing the spawn area.
* Compose: `curseforge:`/`cf:` entries in `mods:` are rejected with a clear error instead of
  being read as Modrinth project `curseforge`.
* Installer: Quilt uses the latest stable installer, not a pre-release.
* Player names in `ops`/`whitelist` are validated before the Mojang lookup.
* `cubeharbor ping` / operator status pings no longer panic (debug) on a negative length from a
  misbehaving server.
* Operator: the `cubeharbor-gateway-ports` Service is no longer deleted when the last allocated
  port goes away. It keeps a placeholder port instead, so a cloud load balancer keeps its
  external IP and voice chat / Bedrock addresses keep working when ports come back.
* Installer: removing the modpack from a server now deletes the pack's mods instead of leaving
  them untracked in `mods/`.
* Installer: players removed from `ops` or `whitelist` in the spec are now removed from
  `ops.json` / `whitelist.json` (players added in-game are left alone), and emptying the
  whitelist turns `white-list` off again. Only players removed after this version are cleaned up.
* Hostname conflicts: when two servers claim the same hostname, or more than one sets
  `routing.default`, the oldest keeps it. The newer one keeps running without the contested hostnames (or default
  flag) and reports the conflict in its status message. The gateway and mc-router always route
  to the oldest instead of picking at random.
* Operator: Ingresses for ports that were removed, renamed or moved to another host are now
  deleted, and turning `rconNetworkPolicy` off deletes the existing per-server RCON
  NetworkPolicies.
* Operator: port names and container ports are checked before anything is applied, so colliding
  or invalid ports (e.g. two long names cut to the same 15 characters, or an extra port reusing
  the voice chat port) give a clear status message instead of repeated API errors.
* Server names that start with a digit (e.g. `2b2t`) are prefixed with `mc-` on compose import
  and quick create, and the operator reports invalid or too long names (Services need a
  DNS-1035 label of at most 52 characters here) instead of failing halfway through a reconcile.
* Compose import: `LEVEL` and `SEED` keep their case, so an imported world folder like
  `MyWorld` is loaded instead of a new world being generated.
* Port allocation: a new or newly pinned port no longer collides with a port the server already
  holds, which left the server in `Error` with "requested twice".
* Installer: malformed hashes in a modpack manifest are rejected instead of crashing the
  installer, and parallel downloads to `a.jar` and `a.zip` no longer share a temp file.
* Operator: status fields that become empty are now cleared, so a fixed error's message and a
  stopped server's player list no longer stick around.

### Security

* Gateway: no longer allowed to patch MinecraftServers. Wake-ups go through a `<release>-wake`
  ConfigMap, the only object the gateway can write, and the operator applies them. A
  compromised gateway can no longer change any server's spec.
* Dashboard: refuses to start with an empty `DASHBOARD_PASSWORD`. Failed logins are now
  rate-limited per client (10/min) with a global backstop (100/min), so one client can no
  longer lock everyone out. New `dashboard.trustedProxies` / `TRUSTED_PROXIES`, and the global
  limit is configurable (`dashboard.loginMaxFailuresTotal` / `LOGIN_MAX_FAILURES_TOTAL`, 0 = off).
* Gateway: forwarded TCP ports (voice chat, Bedrock over TCP, extras) now have a connection
  limit (`gateway.maxForwardConnections`, default 1024, separate from the Minecraft port's), and TCP connections idle in both directions for
  `gateway.idleTimeoutSeconds` (default 600, `IDLE_TIMEOUT_SECS`) are closed.
* Installer: `.mrpack` file downloads are restricted to HTTPS on the hosts the Modrinth pack
  format allows; other URLs are ignored, and a file with none left fails the install
  (`CUBEHARBOR_MRPACK_ANY_HOST=true` to opt out). Installer boolean env vars now accept
  `1`/`yes`/`y`/`on` as well as `true`.

## [0.1.0]

First public release.

* Operator for `MinecraftServer` resources: version resolution, Java image selection, install
  plans, public port allocation, auto-sleep.
* Hostname-routing gateway with sleeping-server status replies, wake-on-join, and TCP/UDP
  forwarding for voice chat and Bedrock. Optional itzg/mc-router mode.
* Installer for vanilla, Paper, Purpur, Fabric, Quilt, Forge, NeoForge, Modrinth and CurseForge
  modpacks, with client-only mod detection.
* Dashboard: quick create, compose import/export, live logs, RCON console, `/api/v1`, and
  customizable branding/theme (`dashboard.site`).
* `cubeharbor` CLI and a Helm chart.
* Cluster portability settings: IPv6/dual-stack (`network.*`), OpenShift-friendly security
  contexts, NodePort-mapped allocated ports, server pod defaults (`serverPods.*`: pull secrets,
  node selection, tolerations, priority), gateway PodDisruptionBudget and topology spread.
* Hardened defaults: `spec.image` overrides are off unless `minecraft.allowImageOverride` is
  set, RCON is limited to the dashboard by a NetworkPolicy, and dashboard sessions live in memory
  (users log in again after the dashboard restarts).
