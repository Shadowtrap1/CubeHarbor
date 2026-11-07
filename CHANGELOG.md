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
* Chart: `commonLabels` (added to every object and pod the chart creates) and `podLabels` /
  `podAnnotations` for the operator, gateway, dashboard, mc-router and cloudflared pods, e.g. for
  log shippers, service meshes or cost labels. The selector labels always win, so these can't
  break a Deployment's selector. cloudflared also gets a PodDisruptionBudget when it runs more
  than one replica (the default 2; `cloudflareTunnel.pdb`).
* Chart: optional Cloudflare Tunnel for reaching the dashboard from outside your network
  without opening a router port (`cloudflareTunnel.enabled`, off by default). It runs a pinned
  `cloudflare/cloudflared` Deployment with a remotely-managed tunnel whose token comes from an
  existing Secret (`existingSecret`; never a plain value), or with `mode: local`, a
  locally-managed tunnel from a `credentials.json` Secret plus ingress rules the chart renders
  from values. With the tunnel on, the session cookie is `Secure` and `cloudflareTunnel.hostname`
  is reserved (servers can't claim it with `expose: ingress`, and DDNS never manages it). The README covers the Cloudflare setup, Cloudflare
  Access, and setting `dashboard.trustedProxies` to the pod CIDR so failed logins are limited per
  visitor. Player traffic does not use the tunnel. See `examples/values-cloudflare-tunnel.yaml`.
* Dashboard: **users and roles**. Each user has their own password and a role: `viewer`
  (read-only), `operator` (also start/stop/restart and console) or `admin` (everything,
  including users). Roles are enforced by the server on every page and `/api` route. Admins
  manage users on a new **Users** page (add, remove, change role, reset password; there is always
  at least one admin). Passwords are hashed with argon2id instead of unsalted SHA-256. Users are
  kept in a new `<release>-dashboard-users` Secret, the only one the dashboard can read and write
  (`dashboard.manageUsers`, on by default; it now also works with `existingSecret`).
* Dashboard: per-user **API tokens** (`Authorization: Bearer chb_...`), created and revoked on
  the Account page and stored hashed. A token has its user's role.
* Chart: `values.schema.json`, so mistakes fail at `helm install`/`upgrade` time with a clear
  message instead of rendering something broken: unknown top-level keys (typos like
  `dashbaord`), wrong types, and invalid enums (`router.mode`, Service `type`,
  `externalTrafficPolicy`, `image.pullPolicy`, `cloudflareTunnel.mode`/`protocol`, IP families),
  plus `minecraft.portRange` and `cloudflareDdns.interval` formats and `operator.replicas: 1`.
  Nested objects stay open, so Kubernetes fields (affinity, tolerations, securityContext, server
  specs) pass through unchanged. Use `--set-string` for a `dashboard.password` that looks like a
  number.
* Chart: `dashboard.networkPolicy` admits only cloudflared and `allowFrom` (e.g. the ingress
  controller) to the dashboard. It's on by default with `cloudflareTunnel`, so other pods
  (including Minecraft servers running third-party plugins) can't forge `X-Forwarded-For` from
  inside `dashboard.trustedProxies` to dodge the per-client login limit.
* Dashboard: an **Account** page to change the login password (current password required,
  at least 12 characters). It's saved to the dashboard's Secret and logs out other sessions
  (`dashboard.allowPasswordChange`, on by default; off with `existingSecret`). The README now
  explains choosing your own password at install.
* CI: a `chart-check` job (`scripts/chart-check.sh`, also `make chart-lint`) runs `helm lint
  --strict` and validates the rendered manifests with kubeconform for Kubernetes 1.27 and 1.33,
  with the default values, every `examples/values-*.yaml` and new `charts/cubeharbor/ci/*-values.yaml`
  combinations (hostNetwork, mc-router, everything enabled). It also fails when the chart
  references an image that `docker-bake.hcl` doesn't build, so a release can't publish a chart
  pointing at a missing image. Releases now wait for it.

### Changed

* **Breaking (dashboard):** log in with a user name. On upgrade, the existing password becomes
  user `admin`; after that `dashboard.password` is only used to recreate `admin` when the users
  Secret is empty (see the README for recovering a locked-out install). The password is no
  longer accepted as a `Bearer` token on `/api` (such requests get `401` with a hint saying so).
  To migrate a script: log in, open **Account → API tokens**, create a token (as a `viewer` user
  if the script only reads), and send `Authorization: Bearer chb_...` instead of the password. A
  password change no longer writes `dashboard.password`'s Secret; `PASSWORD_SECRET` is replaced
  by `USERS_SECRET`, and `dashboard.allowPasswordChange` by `dashboard.manageUsers` (setting
  `allowPasswordChange: false` still turns it off).

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

* Chart: mc-router (`router.mode: mc-router`) no longer enables its HTTP API on port 8080.
  That API is unauthenticated and can add or replace routes, so any pod in the cluster could
  have redirected a hostname's players to a server of its choosing. Nothing used it.
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
