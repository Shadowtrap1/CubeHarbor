# Roadmap

Proposed features, roughly in the order they'd pay off: most player or operator value for the least
effort first. Effort is S (days), M (a week or two) or L (several weeks). This is a set of proposals,
not a commitment.

Already shipped, so not listed: hostname routing, sleep and wake, Geyser/Floodgate and voice chat
ports, modpack installs, compose files, Cloudflare DNS and Tunnel, users and roles, read-only remote
dashboards (`remotes`), metrics, leader election and server templates. World backups are being built
separately; several items below build on them.

| # | Feature | Value | Effort | Needs |
|---|---|---|---|---|
| 1 | [Scheduled restarts with in-game warnings](#1-scheduled-restarts-with-in-game-warnings) | High | S | |
| 2 | [Webhooks and Discord](#2-webhooks-and-discord) | High | S | |
| 3 | [Mod and modpack update tracking](#3-mod-and-modpack-update-tracking) | High | M | |
| 4 | [Staged template rollouts](#4-staged-template-rollouts) | High | M | 1 |
| 5 | [Per-server metrics and TPS](#5-per-server-metrics-and-tps) | Medium | S | |
| 6 | [Demand schedules and pre-warming](#6-demand-schedules-and-pre-warming) | Medium | S | |
| 7 | [Operator scale-up](#7-operator-scale-up) | Medium | S | |
| 8 | [Velocity networks](#8-velocity-networks) | Very high | L | |
| 9 | [Git-driven config](#9-git-driven-config) | High | M | |
| 10 | [Marketplace install](#10-marketplace-install) | High | M | templates |
| 11 | [Crash diagnosis](#11-crash-diagnosis) | High | M | |
| 12 | [Cost reporting](#12-cost-reporting) | Medium | M | 5 |
| 13 | [Clone and staging servers](#13-clone-and-staging-servers) | Medium | M | backups |
| 14 | [Dashboard single sign-on](#14-dashboard-single-sign-on) | Medium | M | |
| 15 | [Fleet autoscaling](#15-fleet-autoscaling) | High | L | 8 |
| 16 | [Multi-cluster placement](#16-multi-cluster-placement) | Medium | L | 7, backups |

## 1. Scheduled restarts with in-game warnings

Long-running modded servers leak memory, so owners restart them nightly by hand. Add
`spec.restart: { cron, timezone, warnAt: [10m, 1m], skipIfPlayersOnline }`. A small operator loop
sends `say` over the existing RCON client, then sets `RESTART_ANNOTATION`. The same warn-and-restart
step becomes the primitive that makes template rollouts and mod updates polite.

## 2. Webhooks and Discord

Owners want to know when a server crashes, without watching a dashboard. A `notifications` Helm block
and per-server `spec.notify` send phase transitions, wake-ups, install failures and player join or
leave. Build it as an event bus inside the operator, fed by status transitions and the status
conditions work, with sinks for generic signed webhooks and Discord embeds. Keep URLs in Secrets. A
later Discord bot can offer `/start`, going through the dashboard's API tokens.

## 3. Mod and modpack update tracking

Nothing tells an owner that a pack or plugin has a new release, or whether it's safe. A slow
background loop (hourly, shared cache per project) compares `status.resolved` and each Modrinth mod
against upstream for the same Minecraft version and loader, and publishes `status.updates`. The
dashboard shows a badge and a one-click bump that edits the spec. Security-flagged releases are
highlighted. An opt-in `autoUpdate: patch` policy applies them in the restart window from item 1, with
a backup first.

## 4. Staged template rollouts

The operator already restarts running servers one template slot at a time
(`TEMPLATE_ROLLOUT_MAX_UNAVAILABLE`), but a waiting server skips its whole reconcile until its turn.
Add `rollout: { maxUnavailable, waitForEmpty, window }` on the template, and snapshot the merged
result so a waiting server keeps reconciling on the previous revision. Add a template watch here
too, so edits start in seconds rather than up to a minute.

## 5. Per-server metrics and TPS

Metrics stop at fleet totals. Export per-server players, phase, restarts and TPS/MSPT (RCON `tps` on
Paper, `forge tps` on Forge, Spark where installed) from the ping loop, and ship a Grafana dashboard
with the chart's ServiceMonitor. Bounded by server count, so label cardinality is safe.

## 6. Demand schedules and pre-warming

Modpacks take minutes to wake, which is what stops owners from using sleep. `spec.schedule` holds
always-on windows ("Fri 18:00-Sun 23:00") during which sleep is suppressed. Later, learn from
`lastActive` history and start servers 10 minutes before their usual peak. Cheap, because it only
feeds the existing `sleeping` decision in reconcile.

## 7. Operator scale-up

Prerequisite for very large installs. Read allocations and routing claims from the controller's
reflector cache and take the authoritative list only when a port is really being allocated. Move
player pings to their own loop. Add `dashboard` caching. See the limits in
[EXTENDING.md](EXTENDING.md#known-scaling-limits). Pair it with the kind/k3d integration suite the
README calls the biggest gap.

## 8. Velocity networks

The single biggest gap against hosted networks: compose can group servers, but players can't move
between them. A `MinecraftNetwork` CRD declares a Velocity proxy Deployment, backends selected by
label, a lobby and a forwarding secret. The operator renders `velocity.toml`, generates the secret,
sets each backend to modern forwarding (`paper-global.yml`, FabricProxy-Lite, or Ambassador for Forge),
and closes backends to direct joins using the existing NetworkPolicy. Velocity starts sleeping
backends through the wake path. A single shared Geyser on the proxy gives Bedrock players the whole
network on one port.

## 9. Git-driven config

`GitSource` (repo, branch, path, deploy-key Secret, interval) syncs compose files and templates through
the existing `compose::parse` and apply path, prunes what was removed, and reports the applied commit.
Works without Argo or Flux, and doubles as change history and rollback. Signed-commit checks optional.
The dashboard shows Git-managed servers as read-only.

## 10. Marketplace install

A browsable catalog in the dashboard: "Vanilla+ SMP", "Create", "Skyblock", "Minigame lobby". Each
entry is a versioned `MinecraftServerTemplate` from a signed index (official, plus user-added URLs),
installed with one click and tuned by a small form. Include recommended memory from pack size and
one-click community template publishing. Treat third-party indexes as untrusted: never accept `image`,
`env` or `files` from them without showing the owner.

## 11. Crash diagnosis

"Server keeps crashing (see the logs)" is where most owners give up. The runner saves `crash-reports/`
and the last log lines to a ConfigMap, and a rules engine maps known signatures to plain advice: wrong
Java, client-only mod, out of memory, missing dependency. Name the suspect mod, offer to add it to
`modpack.exclude`, and feed the advice into status and notifications. Rules update without a release.

## 12. Cost reporting

Sleep's benefit is invisible today. Accumulate running hours and requested CPU, memory and storage for
each server, with price inputs per unit or from OpenCost if installed. Dashboard: cost per server and
per player-hour, and "auto-sleep saved you X this month". Also flag servers that stay idle and memory
far above the observed peak.

## 13. Clone and staging servers

Built on world backups: `spec.dataFrom: { backup | server }` seeds a new volume so an owner can test
a pack upgrade on a copy of the real world, then promote it. Adds a dashboard "Clone" button and
automatic hostnames such as `staging-<name>`.

## 14. Dashboard single sign-on

OIDC login (Google, Discord, Authentik) mapped to the existing viewer, operator and admin roles, and
per-namespace role scopes so communities can share one install safely. Pair with an audit log of who
did what.

## 15. Fleet autoscaling

Needs item 8. A `MinecraftServerFleet` runs N identical stateless servers from a template behind the
proxy, scaling on players per instance with a buffer, and draining before removal. Suits lobbies and
minigames, which is where horizontal scale is real. Worlds stay single-pod, so survival servers keep
their vertical path: recommend or apply memory changes from observed usage.

## 16. Multi-cluster placement

Dashboards can already view remotes. Go further with a hub that places servers by capacity and region,
moves a server by restoring its latest backup in another cluster, and offers global DNS to the nearest
gateway. Only worth it after operator scale-up and backups, and for hosts running real regions.
