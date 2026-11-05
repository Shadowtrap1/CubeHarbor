# cubeharbor

[![ci](https://github.com/Shadowtrap1/cubeharbor/actions/workflows/ci.yml/badge.svg)](https://github.com/Shadowtrap1/cubeharbor/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Minecraft servers on Kubernetes, built in Rust. One Helm chart installs an operator, a
hostname-routing gateway and a web dashboard. You describe servers (or a whole network) in a
compose file, a form, or a `MinecraftServer` resource. cubeharbor then resolves versions,
installs the loader, the modpack and its mods, allocates public ports for voice chat and
Bedrock, wires up routing, and puts idle servers to sleep.

> **Status:** early (0.x). The `MinecraftServer` API is `v1alpha1` and may still change between
> minor versions. Check [CHANGELOG.md](CHANGELOG.md) before upgrading.

## How this project was made, and how you can help

**Most of cubeharbor was written with an AI model** (Anthropic's Claude), directed and reviewed
by one person. That covers most of the code, the Helm chart, the security fixes and this
documentation. The unit tests pass and the chart renders for the cluster setups described below,
but much of it hasn't been run against real clusters, real modpacks and real players.

So it needs more people. Most useful right now:

* **Run it and report what breaks.** Your Kubernetes distribution, modpacks that don't install,
  mods that crash, and anything the docs get wrong.
* **Review the code.** AI-written code can look right and still be subtly wrong, so experienced
  Rust, Kubernetes and Minecraft-server reviewers are especially welcome. Treat it as a
  0.x project and read [Security notes](#security-notes) before exposing it to the internet.
* **Send fixes and tests.** Integration tests against a real cluster (kind/k3d) are the biggest gap.

See [CONTRIBUTING.md](CONTRIBUTING.md) to get started. Contributions are welcome whether you write
them yourself or with AI help, as long as you've tested them.

```
                 players
                    │  play.example.com / atm.example.com     voice / bedrock (UDP)
                    ▼                                          ▼
         ┌───────────────────────────────────────────────────────────────┐
         │ cubeharbor-gateway  (Rust)                                     │
         │  :25565  reads the MC handshake → routes by hostname          │
         │          answers pings for sleeping servers, wakes on join    │
         │  :256xx  TCP/UDP forwarders for allocated ports               │
         └──────────────┬──────────────────────────────┬─────────────────┘
                        │ Service <name>:25565          │ Service <name>:24454/udp …
         ┌──────────────▼──────────────┐   ┌────────────▼─────────────────┐
         │ StatefulSet <name>          │   │ StatefulSet <other>          │
         │  init: cubeharbor-installer  │   │  …                           │
         │  main: cubeharbor-runner+JVM │   └──────────────────────────────┘
         │  PVC  data-<name>-0         │
         └──────────────▲──────────────┘
                        │ creates / updates
         ┌──────────────┴──────────────┐     ┌──────────────────────────────┐
         │ cubeharbor-operator (Rust)   │◄────│ cubeharbor-dashboard (Rust)   │
         │  watches MinecraftServer    │ CRs │  forms, compose import, logs │
         │  resolves versions & Java   │     │  RCON console, modpack search│
         │  allocates public ports     │     └──────────────────────────────┘
         │  manages gateway port svc   │
         └─────────────────────────────┘
```

## Components

| Crate | Binary / image | Role |
|---|---|---|
| `crates/core` | – | CRD types, compose parser, Minecraft protocol (handshake, SLP, PROXY v2), RCON, Mojang/Modrinth/CurseForge clients, version resolution |
| `crates/operator` | `cubeharbor-operator` | Reconciles `MinecraftServer` → Secret, ConfigMap (install plan), Service, StatefulSet, Ingress. Port allocation, status, player polling, auto-sleep, gateway port Service |
| `crates/gateway` | `cubeharbor-gateway` | Hostname routing on 25565 (Forge/FML suffixes handled), offline/sleeping status replies, wake-on-join, dynamic TCP/UDP forwarders |
| `crates/dashboard` | `cubeharbor-dashboard` | Server-rendered UI (axum + maud): list/start/stop/restart/delete, quick create with modpack search, compose preview/apply, live logs (SSE), RCON console, spec editor, JSON API |
| `crates/installer` | in `cubeharbor-server` | Init container: installs vanilla/Paper/Purpur/Fabric/Quilt/Forge/NeoForge, Modrinth `.mrpack`, CurseForge packs (server pack or manifest), extra mods with dependency resolution, writes configs |
| `crates/runner` | in `cubeharbor-server` | PID 1: sizes the heap from the cgroup limit, Aikar's flags, forwards stdin, turns SIGTERM into `stop` so worlds save |
| `crates/cli` | `cubeharbor` | CLI: `crd`, `render`, `resolve`, `apply`, `ping`, `rcon` |

The server image is `cubeharbor-server:<tag>-java{8,17,21,25}`. The operator picks the variant
using the Java version Mojang publishes for the resolved Minecraft version.

## Quick start

**You need:** Kubernetes 1.27+ (k3s, kind, a managed cluster, ...), Helm 3.8+, a default
StorageClass for world volumes, and a LoadBalancer implementation (cloud LB, MetalLB, k3s
ServiceLB) or `gateway.hostNetwork: true`. You also have to accept the
[Minecraft EULA](https://aka.ms/MinecraftEULA).

```sh
# 1. Install the chart (images come from ghcr.io/shadowtrap1)
helm install cubeharbor oci://ghcr.io/shadowtrap1/charts/cubeharbor --version 0.1.0 \
  -n cubeharbor --create-namespace \
  --set minecraft.acceptEula=true \
  --set minecraft.baseDomain=mc.example.com \
  --set curseforge.apiKey=$CF_API_KEY            # optional, for CurseForge packs

# 2. Point a wildcard DNS record *.mc.example.com at the gateway's external IP
kubectl -n cubeharbor get svc cubeharbor-gateway

# 3. Open the dashboard and log in with the generated password
kubectl -n cubeharbor port-forward svc/cubeharbor-dashboard 8080:80
kubectl -n cubeharbor get secret cubeharbor-dashboard -o jsonpath='{.data.password}' | base64 -d; echo
```

To choose the dashboard password yourself, add `--set-string dashboard.password='<at least 12
characters>'` to the install (or `helm upgrade --reuse-values` later), or keep it in your own Secret
with a `password` key and set `dashboard.existingSecret`. Without either, a random 24-character
password is generated once and kept across upgrades.

Before you expose the dashboard, read [Security notes](#security-notes) and give it an Ingress
with TLS (`dashboard.ingress`). [`examples/values-k3s.yaml`](examples/values-k3s.yaml) is a
complete values file for a home-lab cluster. Every option is documented in
[`charts/cubeharbor/values.yaml`](charts/cubeharbor/values.yaml).

The CLI is optional:

```sh
cargo install --locked --git https://github.com/Shadowtrap1/cubeharbor cubeharbor
```

**From source** (your own registry, or before a release exists):

```sh
make push REGISTRY=ghcr.io/you TAG=0.1.0         # docker buildx bake
helm install cubeharbor charts/cubeharbor -n cubeharbor --create-namespace \
  --set image.registry=ghcr.io/you --set image.tag=0.1.0 \
  --set minecraft.acceptEula=true --set minecraft.baseDomain=mc.example.com
```

Then create a server from the dashboard, or apply a compose file:

```sh
cubeharbor apply examples/network.yaml
kubectl get mcs -A
# NAME      PHASE        VERSION  LOADER  PLAYERS  ADDRESS
# atm10     Installing   1.21.1   neoforge         atm.mc.example.com
# lobby     Running      1.21.4   paper   3        lobby.mc.example.com
```

### Cluster compatibility

Defaults suit a typical cluster with a load balancer and a default StorageClass. For anything
else, the setting to change is in [`charts/cubeharbor/values.yaml`](charts/cubeharbor/values.yaml):

| Your cluster | Set |
|---|---|
| Cloud (EKS, GKE, AKS, ...) | Defaults. Add `gateway.service.annotations` for your cloud's LB options, and `minecraft.storageClass` for faster disks |
| k3s / home lab | [`examples/values-k3s.yaml`](examples/values-k3s.yaml). ServiceLB or MetalLB provides the LoadBalancer |
| Bare metal without a load balancer | `gateway.hostNetwork: true` (gateway binds 25565 and the allocated ports on its node, so pin it with `gateway.nodeSelector`; the namespace must allow host networking). Or `gateway.service.type: NodePort` with a `minecraft.portRange` inside 30000-32767: allocated ports then use the same number as node port |
| IPv6-only or dual-stack | `network.bindAddress: "::"`, plus `network.ipFamilyPolicy: PreferDualStack` and `network.ipFamilies: [IPv4, IPv6]` for dual-stack |
| OpenShift | `podSecurityContext: {}` and `serverPods.securityContext: {}`, so the restricted SCC assigns UIDs |
| Pod Security "restricted" namespaces | Defaults. Every pod, including the server pods, meets the restricted profile |
| Private registry / air-gapped | `image.registry`, `imagePullSecrets` (also used by server pods) and `mcRouter.image`. Modpacks still download from Modrinth/CurseForge/Mojang, so allow egress or use `cache.enabled` |
| Dedicated or tainted game nodes | `serverPods.nodeSelector`, `serverPods.tolerations`, `serverPods.affinity`, `serverPods.priorityClassName` |
| ARM64 (Raspberry Pi, Graviton, Ampere) | Defaults. Images are built for `linux/amd64` and `linux/arm64`. Lower `minecraft.defaultCpuRequest` on small nodes |
| Shared cluster / one namespace | `minecraft.watchNamespace` and `dashboard.allowedNamespaces` limit what cubeharbor sees and touches |
| Many players | `gateway.replicas: 2+` (a PodDisruptionBudget is added) with `gateway.topologySpreadConstraints`. Servers scale individually through `resources.memory`/`cpu` |

The operator always runs as one replica, because it coordinates port allocation in-process.
It's a light controller, so that isn't a bottleneck.

What has been checked: unit tests, and `helm template` for each row above. What hasn't: real
installs on most of these platforms. If you run one, please
[open an issue](https://github.com/Shadowtrap1/cubeharbor/issues) to say whether it worked.

### Upgrading and uninstalling

```sh
# Helm installs CRDs once and never updates them. Apply the new CRD first:
kubectl apply --server-side -f charts/cubeharbor/crds/minecraftservers.yaml
helm upgrade cubeharbor oci://ghcr.io/shadowtrap1/charts/cubeharbor --version <new> -n cubeharbor --reuse-values
```

`helm uninstall cubeharbor -n cubeharbor` removes the operator, gateway and dashboard. Servers
you created yourself and all world volumes stay (servers defined under `servers:` in the Helm
values are part of the release and get deleted). Delete the servers with `kubectl delete mcs --all -A` (volumes
`data-<name>-0` are still kept, remove them with `kubectl delete pvc` when you're sure). Then
delete the CRD with `kubectl delete crd minecraftservers.cubeharbor.dev`.

## Compose files

A compose file describes a set of servers. Every field of the `MinecraftServer` spec is
accepted, plus these shorthands:

```yaml
name: my-network            # label applied to every server
namespace: cubeharbor
defaults: { acceptEula: true, memory: 6Gi }   # merged into every server
servers:
  atm10:
    modpack: curseforge:all-the-mods-10       # or modrinth:slug@version, a modpack page URL, or a .mrpack/.zip URL
    memory: 14Gi                              # container limit; heap = limit - max(1Gi, 15%)
    storage: 60Gi
    voice: true                               # or simple-voice-chat / plasmo-voice / {kind, publicPort, install}
    geyser: true                              # Bedrock cross-play (Geyser + Floodgate)
    autoSleep: 20                             # idle minutes
    hostname: atm.example.com
    mods: [lithium, "spark@1.10.0", https://example.com/x.jar]
    ports: ["bluemap:8100@map.example.com", "query:25565/udp", "metrics:9100!internal"]
```

**docker-compose import.** Files with a `services:` section that use `itzg/minecraft-server`
are converted: `TYPE`, `VERSION`, `MODRINTH_MODPACK`, `CF_SLUG`/`CF_PAGE_URL`/`CF_FILE_ID`,
`MEMORY`, `MODRINTH_PROJECTS`, `MODS`/`PLUGINS`, `OPS`, `WHITELIST`, the common
`server.properties` variables, JVM options, and port mappings. UDP 24454 becomes voice chat,
UDP 19132 becomes Geyser, and other ports become extra ports pinned to the same host port.
`mc-router`, proxies and backup sidecars are skipped with a warning. Run
`cubeharbor render docker-compose.yml` to see what will be created.

**Importing a whole docker-compose setup.** Pass several files at once (or pick a `servers/`
folder in the dashboard's Compose tab). They are imported together, and a file that isn't a
Minecraft server (Terraria, ...) is skipped with a warning instead of failing the import.
itzg setups often call the service `mc` in every file, so the server name comes from
`container_name`, the first `mc-router.host` label, the `/data` volume's folder, or the
compose file's folder, in that order. `--rehost` (the "Give imported servers
`<name>.<baseDomain>`" checkbox in the dashboard) drops hostnames that belong to the old setup:

```sh
cubeharbor render ~/Minecraft/servers/*/compose.yaml --rehost mc.example.com   # preview
cubeharbor apply  ~/Minecraft/servers/*/compose.yaml --rehost mc.example.com
```

World data is not copied. Each warning names the old data folder and the new volume
(`data-<name>-0`).

**Exporting.** Every server can be written back out as a cubeharbor compose file: use
"Compose file" on a server page, "Export compose" for everything, or "Start from current
servers" in the Compose tab to copy an existing server into a new one.

## API (for central dashboards)

Log in with the dashboard password as a `Bearer` token (or a session cookie). Browsers on other
origins need `dashboard.corsOrigins`. `dashboard.publicServerList: true` makes the list readable without a login.

| Endpoint | |
|---|---|
| `GET /api/v1/servers` | Every server: cubeharbor's own plus each `dashboard.remotes` setup. Use `?remotes=false` for cubeharbor's only |
| `POST /api/v1/compose` | Body: a compose file (or several joined with `---`). `?dry_run=true` previews, `?rehost=true` rehosts |
| `GET /api/v1/compose` | All servers as one compose file (`?project=`, `?namespace=`) |
| `GET /api/v1/servers/{ns}/{name}/compose` | One server as a compose file |

```json
{ "updated": 1793745337,
  "servers": [
    { "source": "cubeharbor", "name": "lobby", "namespace": "cubeharbor", "phase": "Running", "online": true,
      "address": "lobby.mc.example.com", "version": "1.21.4", "loader": "paper",
      "players": { "online": 3, "max": 20 }, "memory": "4Gi",
      "ports": [{ "name": "voice", "protocol": "UDP", "address": "mc.example.com:25600" }],
      "links": { "dashboard": "/servers/cubeharbor/lobby", "compose": "/api/v1/servers/cubeharbor/lobby/compose" } },
    { "source": "games", "name": "voxy", "phase": "Offline", "online": false, "address": "voxy.mc.example.com" } ],
  "totals": { "servers": 2, "online": 1, "players": 3 },
  "sources": [{ "name": "cubeharbor", "ok": true }, { "name": "games", "ok": true }] }
```

The top level (`updated`, `servers[].name/address/online`, `totals`) matches the
games-dashboard `/api/servers` format, so one page can read either. A remote can be a
games-dashboard, another cubeharbor (`/api/v1/servers`), or any URL that returns a JSON list of
`{name, address, online, players, version}`.

```sh
curl -H "Authorization: Bearer $PW" https://cubeharbor.example.com/api/v1/servers
curl -H "Authorization: Bearer $PW" --data-binary @network.yaml 'https://cubeharbor.example.com/api/v1/compose?dry_run=true'
```

## Large modpacks

* **Parallel, verified downloads.** Pack files download 16 at a time (`minecraft.downloadConcurrency`)
  and are checked against their SHA-1/SHA-512 hashes. Files already on disk are skipped, so
  restarts and pack updates only fetch what changed.
* **Client-only mods are removed automatically.** Every pack file is identified by hash on
  Modrinth (this works for CurseForge packs too). Files whose project is marked
  `server_side: unsupported` are skipped, as are resource packs, shaders, and a built-in list of
  known crashers (Embeddium, Oculus, OptiFine, …). Mods that a kept mod *requires* are never
  dropped. Use `modpack.exclude` / `modpack.include` for anything the detection gets wrong.
  Tested with Create+ (257 files, Forge 1.19.2): 55 client-only files were removed and the
  server booted without manual changes.
* **CurseForge server packs** are preferred when the author publishes one (`useServerPack: false` to opt out).
* **Incremental updates.** Changing the pack version re-installs, removes jars from the old version and re-applies
  the pack's config overrides. Worlds and unrelated configs are kept.
* **Startup budget.** The startup probe allows 60 minutes for the first boot, and the pod gets 120s to save on shutdown.
* **Shared cache** (`cache.enabled`, needs ReadWriteMany): downloads are stored by hash and reused across servers.

## Ports, routing and voice chat

* All Java traffic goes through one port (25565). The gateway reads the hostname from the
  handshake and proxies to `<server>.<ns>.svc`. Servers get `<name>.<baseDomain>` or their
  `routing.hostnames`. One server can be `routing.default: true` to catch unknown hostnames.
* **Voice chat, Bedrock and extra ports** get a public port from `minecraft.portRange`, or a
  pinned `publicPort`. The operator maintains a `cubeharbor-gateway-ports` Service (LoadBalancer
  by default) with every allocation, and the gateway binds the matching TCP/UDP forwarders.
  Allocations are stored in each server's status and stay stable across restarts. Once created,
  the Service is kept even when no ports are allocated (with a placeholder port), so its load
  balancer address doesn't change; delete it by hand to release the load balancer. The placeholder
  is TCP on the first port of `minecraft.portRange` (with `NodePort` it holds that node port), and
  some cloud load balancers report its target unhealthy because nothing listens there; that's
  expected while no ports are allocated.
* **Simple Voice Chat** is installed from Modrinth (Fabric, Forge, NeoForge, Quilt, or the
  Paper plugin) and its `voice_host` is set to `<publicHost>:<allocated port>`, so clients
  connect without manual configuration. Plasmo Voice gets a port too; set its `host.port` to 60606.
* **Geyser + Floodgate** are fetched from GeyserMC for the right platform (Paper, Fabric, NeoForge).
* **Real client IPs:** set `routing.proxyProtocol: true` to send a PROXY v2 header. Paper is
  configured automatically. Modded servers need a proxy-protocol mod. If your load balancer
  sends PROXY to the gateway, set `gateway.acceptProxyProtocol`.
* **Sleeping servers** still show up in the server list ("Sleeping — join to wake it up").
  When someone joins, the gateway starts the server and tells the player to retry in a minute.

### mc-router instead of the gateway

`router.mode: mc-router` deploys [itzg/mc-router](https://github.com/itzg/mc-router) for Java
routing. The operator annotates each server's Service with
`mc-router.itzg.me/externalServerName: <hostnames>` (plus `defaultServer` for
`routing.default`). With `minecraft.watchNamespace` set, mc-router only watches that namespace,
so it can run next to another mc-router on the same cluster. With `mcRouter.autoScaleUp`,
joining a sleeping server scales its StatefulSet to 1. The operator sees this and marks the
server awake. The gateway still forwards voice chat, Bedrock and extra ports. Give
`mcRouter.service.port` a port other than 25565 if something else already owns 25565.
[`examples/values-mc-router.yaml`](examples/values-mc-router.yaml) is a full example on the
standard port 25565, with notes for running next to another setup that already owns 25565.

## Customizing

**Dashboard look and form defaults** go under `dashboard.site` in your Helm values. The chart
renders them into a ConfigMap, and the dashboard restarts when they change. Every key is optional.
[`examples/site.yaml`](examples/site.yaml) lists all options with their defaults:

```yaml
dashboard:
  site:
    name: Blockhaus
    icon: "🧱"                       # or logoUrl: https://…
    theme: { accent: "#7c5cff", bg: "#101014" }   # any CSS variable from assets/style.css
    customCss: "header { border-bottom-width: 2px }"
    links: [{ label: Map, url: https://map.example.com, newTab: true }]
    newServer: { memory: 6Gi, autoSleepMinutes: 20, voice: true }
```

Outside Kubernetes, pass the same YAML with `SITE_CONFIG=/path/site.yaml` (or `--site-config`).

**Built-in defaults** live in two places:

| What | Where |
|---|---|
| Dashboard stylesheet, scripts, example compose file | `crates/dashboard/assets/` |
| Dashboard branding and form defaults | `crates/dashboard/src/site.rs` |
| Server memory/storage/CPU, auto-sleep minutes, namespace, server image, port range, download concurrency, pod timings, JVM sizing | `crates/core/src/defaults.rs` |

After changing a spec default (memory, storage, idle minutes), run `make crd`.

## Development

```sh
cargo test --workspace          # unit tests (protocol, compose, allocation, installer, …)
make crd                        # regenerate charts/cubeharbor/crds from the Rust types
make chart-lint
docker build --target server --build-arg JAVA_VERSION=21 -t cubeharbor-server:dev-java21 .
```

Local installer + runner smoke test without Kubernetes:

```sh
mkdir -p data etc && cat > etc/plan.json <<'EOF'
{"serverName":"t","minecraftVersion":"1.21.1","loader":"fabric","acceptEula":true,
 "mods":[{"modrinth":"simple-voice-chat"}]}
EOF
docker run --rm -v $PWD/data:/data -v $PWD/etc:/etc/cubeharbor cubeharbor-server:dev-java21 cubeharbor-installer
docker run --rm -it -m 4g -p 25565:25565 -v $PWD/data:/data cubeharbor-server:dev-java21
```

## Security notes

Treat the dashboard password like a cluster credential: it can create and change servers in
every namespace the dashboard manages.

* **Dashboard login.** One password, generated on install and stored in a Secret; also accepted
  as a `Bearer` token for `/api/*`. Logins get a random session token that lasts 7 days and is
  revoked on logout (and on a dashboard restart). The dashboard refuses to start with an empty
  password. A client with 10 failed password checks in a minute gets `429` until the minute is
  over, and so does everyone once 100 failures a minute come in overall
  (`dashboard.loginMaxFailuresTotal`; 0 turns the overall limit off, so nobody can block other
  people's logins). Behind an Ingress, list the ingress controller's pod CIDR in
  `dashboard.trustedProxies` so clients are told apart by `X-Forwarded-For`; otherwise they share
  one limit. Serve the dashboard over TLS: the cookie
  is marked `Secure` automatically when the Ingress has `tls`, or set `dashboard.cookieSecure`.
* **Namespaces.** The dashboard only manages its default namespace plus
  `dashboard.allowedNamespaces`, and its RBAC (including `get` on Secrets for RCON) is granted
  per namespace. `["*"]` restores cluster-wide access. Set `minecraft.watchNamespace` to keep
  the operator's write access to one namespace as well.
* **No takeovers.** The operator refuses to update a Service, StatefulSet, Secret, ConfigMap,
  Ingress or NetworkPolicy that already exists unless the MinecraftServer owns it.
* **Server pods run third-party code** (mods and plugins), so they get no ServiceAccount token,
  no privilege escalation, no capabilities and the default seccomp profile. Only the install
  container sees the CurseForge key, the RCON password and the shared download cache. A
  per-server NetworkPolicy (`minecraft.rconNetworkPolicy`) leaves the game ports open but lets
  only the dashboard reach RCON.
* **Images and hosts.** `spec.image` overrides are off unless `minecraft.allowImageOverride` is
  set (optionally limited by `minecraft.allowedImagePrefixes`). `expose: ingress` hosts must be
  under `baseDomain` (or `minecraft.ingressHostSuffixes`), and the dashboard's host is reserved.
* **Downloads.** Files without a published checksum must come over HTTPS
  (`CUBEHARBOR_ALLOW_INSECURE_HTTP=true` in a server's `env` overrides this). Single downloads are
  capped at 8 GiB and archive extraction at 32 GiB (`CUBEHARBOR_MAX_DOWNLOAD_MIB`,
  `CUBEHARBOR_MAX_EXTRACT_MIB`). Files listed in a `.mrpack` are only downloaded over HTTPS from
  the hosts the pack format allows (`cdn.modrinth.com`, `github.com`, `raw.githubusercontent.com`,
  `gitlab.com`), so a crafted pack can't make the installer call cluster-internal addresses
  (`CUBEHARBOR_MRPACK_ANY_HOST=true` in the server's `env` lifts this). This protects against pack
  *contents*, not against whoever edits the MinecraftServer: `modpack.url` and `mods[].url` are
  still fetched as given (HTTPS unless checksummed), so only let trusted users edit servers.
* **Gateway.** The gateway is the internet-facing component, so it can only *read*
  MinecraftServers. To wake a sleeping server it writes one ConfigMap (`<release>-wake`, the only
  object its RBAC lets it change), and the operator does the actual wake-up; a compromised gateway
  can at most wake sleeping servers. With `gateway.acceptProxyProtocol`, list your load balancers in
  `gateway.trustedProxies`; otherwise any client can claim any source IP. Connections are capped
  separately for the Minecraft port (`gateway.maxConnections`, default 4096) and for all forwarded
  TCP ports together (`gateway.maxForwardConnections`, default 1024), so a flood on e.g. a web map
  port can't block logins; UDP sessions are capped per port (`MAX_UDP_SESSIONS`, default 4096).
  TCP connections with no traffic for `gateway.idleTimeoutSeconds` (default 600) are closed,
  which also cuts quiet WebSocket UIs on forwarded ports; raise it or set 0 if that matters.
* Operator, gateway and dashboard run as non-root with a read-only root filesystem on distroless images.

## Contributing

Issues and pull requests are welcome, see [CONTRIBUTING.md](CONTRIBUTING.md). Please report
security problems privately as described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE). Minecraft is a trademark of Mojang AB; cubeharbor is not affiliated with
Mojang or Microsoft. Servers run the official server software and need the EULA accepted.
