# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Fixed

* Server names that start with a digit (e.g. `2b2t`) are prefixed with `mc-` on compose import
  and quick create, and the operator reports invalid or too long names (Services need a
  DNS-1035 label of at most 52 characters here) instead of failing halfway through a reconcile.

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
