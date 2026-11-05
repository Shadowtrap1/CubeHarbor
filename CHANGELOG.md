# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Fixed

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
