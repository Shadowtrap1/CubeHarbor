# cubeharbor Helm chart

Minecraft servers on Kubernetes: a Rust operator for `MinecraftServer` resources, a gateway that
routes players by hostname (and forwards voice chat / Bedrock ports), and a web dashboard that
builds servers from modpacks and compose files. Project docs:
[github.com/Shadowtrap1/cubeharbor](https://github.com/Shadowtrap1/cubeharbor).

## Quickstart

No values are required:

```sh
helm install cubeharbor oci://ghcr.io/shadowtrap1/charts/cubeharbor -n cubeharbor --create-namespace
```

Then follow the printed notes:

```sh
# Dashboard: log in as "admin" with the generated password
kubectl -n cubeharbor get secret cubeharbor-dashboard -o jsonpath='{.data.password}' | base64 -d; echo
kubectl -n cubeharbor port-forward svc/cubeharbor-dashboard 8080:80     # http://localhost:8080

# Players connect to the gateway's external address on port 25565
kubectl -n cubeharbor get svc cubeharbor-gateway

# Check that the dashboard and the gateway answer
helm test cubeharbor -n cubeharbor
```

Every server must accept the [Minecraft EULA](https://aka.ms/MinecraftEULA), either per server
(`acceptEula: true`) or for all of them with `--set minecraft.acceptEula=true`. Most installs
also set `minecraft.baseDomain` (servers then answer on `<name>.<baseDomain>`; point a wildcard
DNS record at the gateway) and, for CurseForge modpacks, `curseforge.apiKey`.

Pin a version with `--version <x.y.z>` (see the
[releases](https://github.com/Shadowtrap1/cubeharbor/releases) and the
[changelog](https://github.com/Shadowtrap1/cubeharbor/blob/main/CHANGELOG.md)).

## Requirements

* Kubernetes 1.27+ and Helm 3.8+ (OCI charts).
* A default StorageClass for world volumes, or `minecraft.storageClass`.
* For players outside the cluster: a LoadBalancer implementation (cloud LB, MetalLB, k3s
  ServiceLB). Without one (kind, minikube, bare metal), the gateway's `EXTERNAL-IP` stays
  `<pending>`: use `gateway.service.type=NodePort` or `gateway.hostNetwork=true`, or
  `kubectl port-forward svc/cubeharbor-gateway 25565` to try it locally.
  `helm install --wait` also waits for that address, so leave out `--wait` (or change the
  Service type) on such clusters.
* No particular CNI or ingress controller. The NetworkPolicies the chart creates
  (`minecraft.rconNetworkPolicy`, `dashboard.networkPolicy`) are simply not enforced by CNIs
  without NetworkPolicy support.

The project README has a [table of settings per cluster type](https://github.com/Shadowtrap1/cubeharbor#cluster-compatibility)
(cloud, k3s, bare metal, IPv6, OpenShift, air-gapped, ARM64, ...) and ready-made values files in
[`examples/`](https://github.com/Shadowtrap1/cubeharbor/tree/main/examples).

## What gets installed

| Component | Kind | Notes |
|---|---|---|
| operator | Deployment, 1 replica | Reconciles `MinecraftServer` resources. Only one replica reconciles at a time (it allocates public ports in-process): with `operator.replicas: 2+` the others stand by and take over the `<release>-operator` Lease within ~15s (`operator.leaderElection`, on by default). A restart only pauses reconciling; running servers keep running. Health and Prometheus metrics on port 8080 (`/healthz`, `/readyz`, `/metrics`). |
| gateway | Deployment + LoadBalancer Service | Routes Java players by hostname on 25565, answers pings for sleeping servers and wakes them. Scales out with `gateway.replicas` (a PodDisruptionBudget is added above 1). The operator also manages a `<release>-gateway-ports` Service for allocated voice chat / Bedrock ports. Health and Prometheus metrics on port 8081 (`/healthz`, `/readyz`, `/metrics`); on `SIGTERM` it drains connections for up to 25s. |
| dashboard | Deployment + ClusterIP Service | Web UI and API. Expose it with `dashboard.ingress`, `dashboard.httpRoute` or `cloudflareTunnel`, always over TLS. |
| mc-router | Deployment | Only with `router.mode: mc-router`, instead of the gateway's hostname routing. |
| cloudflared | Deployment | Only with `cloudflareTunnel.enabled`. |
| `minecraftservers.cubeharbor.dev` | CRD | In `crds/`, see upgrading below. |

Every pod runs as non-root with a read-only root filesystem, no capabilities and the
`RuntimeDefault` seccomp profile (Pod Security "restricted"), and has resource requests and
memory limits.

## Dashboard login

The first user is `admin`. Its password is, in order of precedence: the `password` key of
`dashboard.existingSecret`, `dashboard.password` (use `--set-string`), or a random 24-character
password generated on first install and kept across upgrades in the `<release>-dashboard`
Secret. After the first login, manage users and API tokens in the dashboard.

## Upgrading

Helm installs the CRD in `crds/` on first install only and never upgrades or deletes it. Apply
the new CRD before upgrading the release:

```sh
helm pull oci://ghcr.io/shadowtrap1/charts/cubeharbor --version <new> --untar -d /tmp/cubeharbor
kubectl apply --server-side -f /tmp/cubeharbor/cubeharbor/crds/
helm upgrade cubeharbor oci://ghcr.io/shadowtrap1/charts/cubeharbor --version <new> -n cubeharbor --reset-then-reuse-values
```

`--reset-then-reuse-values` (Helm 3.14+) keeps your settings and picks up defaults for values
the new version added. Plain `--reuse-values` keeps the old chart's defaults too, so new
templates can fail on missing values; on older Helm, pass your values file with `-f` instead.

Read the [changelog](https://github.com/Shadowtrap1/cubeharbor/blob/main/CHANGELOG.md) first:
before 1.0, minor versions may change values or the `MinecraftServer` API.

## Uninstalling

```sh
helm uninstall cubeharbor -n cubeharbor
```

This removes the operator, gateway and dashboard. `MinecraftServer` resources you created
yourself and every world volume (`data-<name>-0`) stay; servers listed under `servers:` in the
values are deleted with the release (their volumes are kept). To remove everything:

```sh
kubectl delete mcs --all -A                      # servers (volumes are still kept)
kubectl delete pvc -n <namespace> data-<name>-0  # each world you no longer want
kubectl delete crd minecraftservers.cubeharbor.dev
```

## Values

Generated from [`values.yaml`](values.yaml) by `make chart-readme`; the comments there have
the full details.

<!-- values:start -->
| Key | Default | Description |
|---|---|---|
| `image` |  | Image registry/prefix for all cubeharbor images. |
| `image.registry` | `ghcr.io/shadowtrap1` |  |
| `image.tag` | `""` | Defaults to the chart appVersion. |
| `image.pullPolicy` | `IfNotPresent` |  |
| `imagePullSecrets` | `[]` |  |
| `commonLabels` | `{}` | Labels added to every object the chart creates and to its pods (e.g. team, cost center). app.kubernetes.io/* and helm.sh/chart keys are ignored here: the chart sets those itself. |
| `logFormat` | `text` | Log format of the operator, gateway and dashboard: "text" or "json" (one JSON object per line, for log collectors). |
| `metrics` |  | Prometheus metrics. The gateway and dashboard serve /metrics on their health port (8081) and the operator on 8080 (counts only: no hostnames, IPs or server names). `enabled` adds ClusterIP `<release>-gateway-metrics` / `<release>-operator-metrics` / `<release>-dashboard-metrics` Services (never a public Service) for Prometheus to scrape. |
| `metrics.enabled` | `false` |  |
| `metrics.serviceMonitor.enabled` | `false` | Also create a Prometheus Operator ServiceMonitor (needs the monitoring.coreos.com CRDs). |
| `metrics.serviceMonitor.namespace` | `""` | Namespace of the ServiceMonitor (release namespace when empty). |
| `metrics.serviceMonitor.labels` | `{}` | Extra labels, e.g. the `release: <name>` label your Prometheus selects ServiceMonitors by. |
| `metrics.serviceMonitor.interval` | `30s` |  |
| `metrics.serviceMonitor.scrapeTimeout` | `""` |  |
| `podSecurityContext` |  | Pod securityContext of the operator, gateway, dashboard and mc-router pods. On OpenShift set this to {} so the restricted SCC assigns the UID. |
| `podSecurityContext.runAsNonRoot` | `true` |  |
| `podSecurityContext.runAsUser` | `65532` |  |
| `podSecurityContext.runAsGroup` | `65532` |  |
| `podSecurityContext.seccompProfile` | `{ type: RuntimeDefault }` |  |
| `network.bindAddress` | `"0.0.0.0"` | Address every component listens on: "0.0.0.0" (IPv4) or "::" (IPv6-only and dual-stack clusters). |
| `network.ipFamilyPolicy` | `""` | ipFamilyPolicy for every Service, including the gateway ports Service (SingleStack, PreferDualStack, RequireDualStack). Cluster default when empty. |
| `network.ipFamilies` | `[]` | ipFamilies for every Service, e.g. [IPv4, IPv6]. Cluster default when empty. |
| `serverPods` |  | Pod settings for every Minecraft server the operator creates. Use these to run servers on dedicated or tainted game nodes, pull from a private registry, or fit a platform's rules. |
| `serverPods.imagePullSecrets` | `[]` | Added to the global imagePullSecrets (which server pods also get). |
| `serverPods.nodeSelector` | `{}` | Merged with each server's spec.nodeSelector (the server wins), e.g. { kubernetes.io/arch: amd64 }. |
| `serverPods.tolerations` | `[]` |  |
| `serverPods.affinity` | `{}` |  |
| `serverPods.topologySpreadConstraints` | `[]` |  |
| `serverPods.priorityClassName` | `""` |  |
| `serverPods.runtimeClassName` | `""` |  |
| `serverPods.labels` | `{}` |  |
| `serverPods.annotations` | `{}` |  |
| `serverPods.securityContext` | `null` | Replaces the server pod securityContext. null = built-in (uid/gid 1000, Pod Security "restricted"). {} on OpenShift, so the restricted SCC assigns the UID. |
| `minecraft.acceptEula` | `false` | Accept the Minecraft EULA (https://aka.ms/MinecraftEULA) for every server by default. Servers can still set `acceptEula` individually. |
| `minecraft.baseDomain` | `""` | Servers get `<name>.<baseDomain>` as their address unless they set routing.hostnames. Point a wildcard DNS record (*.mc.example.com) at the gateway. |
| `minecraft.publicHost` | `""` | Hostname/IP players use for voice chat and Bedrock ports. Defaults to baseDomain. |
| `minecraft.portRange` | `"25600-25699"` | Public port pool for voice chat, Bedrock and other extra ports. |
| `minecraft.storageClass` | `""` | Default storage class for world volumes (cluster default when empty). |
| `minecraft.downloadConcurrency` | `16` | Parallel downloads when installing modpacks. |
| `minecraft.defaultCpuRequest` | `500m` | CPU request of servers that don't set resources.cpu. Lower it on small clusters (Raspberry Pi, single node); "0" sets no request. |
| `minecraft.watchNamespace` | `""` | Only manage servers in this namespace (empty = all namespaces). |
| `minecraft.ingressClass` | `""` | Ingress class for HTTP extra ports (web maps). |
| `minecraft.ingressHostSuffixes` | `[]` | Domains servers may claim for `expose: ingress` ports (default: baseDomain; any host when both are empty). The dashboard's own host is always refused. |
| `minecraft.allowImageOverride` | `false` | Let `spec.image` replace the cubeharbor server image. Off by default: anyone who can edit a server could otherwise run arbitrary images in its namespace. |
| `minecraft.allowedImagePrefixes` | `[]` | With allowImageOverride, accept only images starting with one of these (empty = any). |
| `minecraft.rconNetworkPolicy` | `true` | Per-server NetworkPolicy: game and extra ports stay open, RCON only from the dashboard. Needs a CNI that enforces NetworkPolicy (k3s does by default). |
| `curseforge.apiKey` | `""` | CurseForge API key (https://console.curseforge.com). Required for CurseForge modpacks. |
| `curseforge.existingSecret` | `""` | Or reference an existing secret with key `apiKey`. |
| `cloudflareDdns.enabled` | `false` | Keep Cloudflare DNS records pointed at your public IPv4 address, for home installs whose ISP changes it. The operator looks the address up every `interval` and only calls Cloudflare when it changed (plus an hourly re-check that repairs records edited in Cloudflare). Records are created when missing and updated when wrong, always as "DNS only" (proxied: false; Cloudflare's proxy can't carry Minecraft). Other records are left alone and nothing is ever deleted. IPv4 (A records) only. |
| `cloudflareDdns.existingSecret` | `""` | Existing Secret with a Cloudflare API token (Zone > DNS > Edit on the zone). Required. kubectl -n <ns> create secret generic cubeharbor-cloudflare-api --from-literal=apiToken=<token> |
| `cloudflareDdns.existingSecretKey` | `apiToken` | Key of the token in existingSecret. |
| `cloudflareDdns.zone` | `""` | Zone name (example.com) or zone ID. Required. |
| `cloudflareDdns.records` | `[]` | Record names to manage (default: `*.<minecraft.baseDomain>`). Add `minecraft.publicHost` here if it's a name in the zone, so voice chat / Bedrock addresses follow too. Names that are CNAMEs (e.g. a Cloudflare Tunnel hostname) and the dashboard's host are never changed. |
| `cloudflareDdns.serverHostnames` | `false` | Also publish servers' own `routing.hostnames` that fall inside the zone (names a wildcard in `records` already covers are skipped). Records of deleted servers are kept. |
| `cloudflareDdns.interval` | `5m` | How often to check the public IP (s, m or h; at least 30s). |
| `cloudflareDdns.ttl` | `1` | TTL of the records, in seconds (1 = Cloudflare's automatic TTL, otherwise 60-86400). |
| `cloudflareDdns.ipLookupUrls` | `[https://api.ipify.org, https://ipv4.icanhazip.com]` | URLs that answer with your public IPv4 as plain text, tried in order. |
| `cache.enabled` | `false` |  |
| `cache.size` | `50Gi` |  |
| `cache.storageClass` | `""` |  |
| `cache.accessMode` | `ReadWriteMany` |  |
| `operator.replicas` | `1` | 2 or more keeps a standby ready to take over within ~15s. Needs leaderElection: only the replica holding the Lease reconciles (port allocation is coordinated inside one process). |
| `operator.leaderElection` | `true` | Elect a leader with a coordination.k8s.io Lease named <release>-operator. With it on, upgrades roll (the new pod takes over when the old one releases the Lease); off = Recreate. |
| `operator.resources.requests` | `{ cpu: 50m, memory: 64Mi }` |  |
| `operator.resources.limits` | `{ memory: 256Mi }` |  |
| `operator.nodeSelector` | `{}` |  |
| `operator.tolerations` | `[]` |  |
| `operator.affinity` | `{}` |  |
| `operator.topologySpreadConstraints` | `[]` |  |
| `operator.priorityClassName` | `""` |  |
| `operator.logLevel` | `info` |  |
| `operator.podLabels` | `{}` | Extra labels and annotations on the operator pods (e.g. for log shippers or service meshes). |
| `operator.podAnnotations` | `{}` |  |
| `router.mode` | `gateway` | Who routes Java players by hostname: `gateway` (cubeharbor-gateway, also answers pings for sleeping servers) or `mc-router` (itzg/mc-router reading Service annotations). With mc-router the gateway still forwards voice chat / Bedrock / extra ports. |
| `mcRouter.image` | `itzg/mc-router:1.54.2` | Pinned for reproducible installs; bump deliberately (https://github.com/itzg/mc-router/releases). |
| `mcRouter.autoScaleUp` | `true` | Start a sleeping server when a player joins (scales its StatefulSet 0 -> 1). |
| `mcRouter.connectionRateLimit` | `""` |  |
| `mcRouter.extraEnv` | `[]` |  |
| `mcRouter.service.type` | `LoadBalancer` |  |
| `mcRouter.service.port` | `25565` | Public Java port. Use something other than 25565 when another router already owns it. |
| `mcRouter.service.nodePort` | `null` | Node port for the Java port when type is NodePort. |
| `mcRouter.service.externalTrafficPolicy` | `Local` |  |
| `mcRouter.service.annotations` | `{}` |  |
| `mcRouter.resources.requests` | `{ cpu: 20m, memory: 32Mi }` |  |
| `mcRouter.resources.limits` | `{ memory: 128Mi }` |  |
| `mcRouter.nodeSelector` | `{}` |  |
| `mcRouter.tolerations` | `[]` |  |
| `mcRouter.affinity` | `{}` |  |
| `mcRouter.topologySpreadConstraints` | `[]` |  |
| `mcRouter.priorityClassName` | `""` |  |
| `mcRouter.podLabels` | `{}` | Extra labels and annotations on the mc-router pods (e.g. for log shippers or service meshes). |
| `mcRouter.podAnnotations` | `{}` |  |
| `gateway.replicas` | `1` | More than one replica is supported; players are spread by the load balancer. |
| `gateway.pdb` |  | PodDisruptionBudget, created when replicas > 1 so node drains keep one gateway up. |
| `gateway.pdb.enabled` | `true` |  |
| `gateway.pdb.minAvailable` | `1` |  |
| `gateway.hostNetwork` | `false` | Bind directly on node ports instead of using a LoadBalancer (bare metal without a LB). |
| `gateway.service.type` | `LoadBalancer` |  |
| `gateway.service.port` | `25565` |  |
| `gateway.service.nodePort` | `null` | Node port for the Java port when type is NodePort. |
| `gateway.service.annotations` | `{}` |  |
| `gateway.service.externalTrafficPolicy` | `Local` | Local preserves player IPs. |
| `gateway.service.loadBalancerIP` | `""` |  |
| `gateway.acceptProxyProtocol` | `false` | Set when the load balancer in front sends PROXY protocol. |
| `gateway.trustedProxies` | `[]` | CIDRs of those load balancers. PROXY headers from other peers are ignored, so players can't spoof their IP. Empty = trust any peer (only safe if nothing else can reach the gateway). |
| `gateway.idleTimeoutSeconds` | `600` | Close player and forwarded TCP connections after this many seconds without traffic (0 = never). Minecraft clients send keep-alives, so this only catches dead connections. |
| `gateway.maxConnections` | `4096` | Concurrent connections on the Minecraft port, and on all forwarded TCP ports together. |
| `gateway.maxForwardConnections` | `1024` |  |
| `gateway.resources.requests` | `{ cpu: 50m, memory: 32Mi }` |  |
| `gateway.resources.limits` | `{ memory: 256Mi }` |  |
| `gateway.nodeSelector` | `{}` |  |
| `gateway.tolerations` | `[]` |  |
| `gateway.affinity` | `{}` |  |
| `gateway.topologySpreadConstraints` | `[]` |  |
| `gateway.priorityClassName` | `""` |  |
| `gateway.podLabels` | `{}` | Extra labels and annotations on the gateway pods (e.g. for log shippers or service meshes). |
| `gateway.podAnnotations` | `{}` |  |
| `dashboard.enabled` | `true` |  |
| `dashboard.password` | `""` | Password of the first admin, user `admin`, at least 12 characters recommended. Generated (and kept across upgrades) when empty: `kubectl get secret <release>-dashboard -o jsonpath='{.data.password}' \| base64 -d`. Set it here (`--set-string dashboard.password=...`), or keep it in your own Secret with a `password` key via `existingSecret`. It is only used while the dashboard has no users yet (first start, or after removing `users.json` from the `<release>-dashboard-users` Secret to recover); after that, change passwords in the dashboard. |
| `dashboard.existingSecret` | `""` |  |
| `dashboard.manageUsers` | `true` | Keep users (roles, argon2id password hashes, API tokens) in the `<release>-dashboard-users` Secret, which the dashboard may get and patch, and manage them on its Users and Account pages. The chart creates that Secret without data, so upgrades and GitOps syncs don't overwrite the users (avoid Argo CD's `Replace=true` for it). Off: one user, `admin` with `password`, nothing changeable from the dashboard and no API tokens. The old `allowPasswordChange: false` also turns this off. |
| `dashboard.defaultNamespace` | `""` | Namespace for servers created from the dashboard (release namespace when empty). |
| `dashboard.allowedNamespaces` | `[]` | Extra namespaces the dashboard may manage (RBAC is granted per namespace). `["*"]` grants cluster-wide access, including get on every Secret: avoid unless you need it. |
| `dashboard.cookieSecure` | `false` | Mark the session cookie Secure. Always on when the dashboard Ingress has TLS or cloudflareTunnel is enabled. |
| `dashboard.remotes` | `[]` | Other setups shown on the dashboard and in /api/v1/servers (read-only). Each url returns a server list: games-dashboard /api/servers or another cubeharbor /api/v1/servers. |
| `dashboard.corsOrigins` | `[]` | Browser origins allowed to call /api (central dashboards on another host). `*` allows any. |
| `dashboard.publicServerList` | `false` | Serve GET /api/v1/servers without logging in (for public status pages). |
| `dashboard.trustedProxies` | `[]` | CIDRs of proxies in front of the dashboard (e.g. the ingress controller's pod network). Their X-Forwarded-For is believed when rate-limiting failed logins per client; without it, all clients behind the proxy share one limit. With cloudflareTunnel, list the cluster's pod CIDR (k3s: 10.42.0.0/16; see `kubectl get nodes -o jsonpath='{.items[*].spec.podCIDR}'`) and keep service.type ClusterIP, so only in-cluster proxies can reach the dashboard. |
| `dashboard.loginMaxFailuresTotal` | `100` | Failed logins per minute across all clients before every login gets 429 (0 = no global limit; each client is always limited to 10 a minute). Lower slows distributed guessing of a weak password; 0 means nobody can block other people's logins. |
| `dashboard.site` | `{}` | Branding, theme and form defaults of the dashboard. Every key is optional; anything left out keeps the built-in default. All options with their defaults: examples/site.yaml. |
| `dashboard.networkPolicy` |  | NetworkPolicy that only lets cloudflared (with cloudflareTunnel) and `allowFrom` connect to the dashboard. Without it, any pod (including Minecraft servers running third-party plugins) could reach it from inside trustedProxies and forge X-Forwarded-For to dodge the per-client login limit. null = on with cloudflareTunnel, off otherwise. Needs a CNI that enforces NetworkPolicy (k3s does by default). kubectl port-forward and the node itself are not affected. |
| `dashboard.networkPolicy.enabled` | `null` |  |
| `dashboard.networkPolicy.allowFrom` | `[]` | More NetworkPolicyPeers allowed in: your ingress controller (required when dashboard.ingress is enabled), or another cubeharbor release in the cluster that lists this dashboard in its `remotes`. No cubeharbor component calls the dashboard itself. |
| `dashboard.networkPolicy.metricsFrom` | `[]` | NetworkPolicyPeers allowed to scrape the dashboard's /metrics when metrics.enabled is on (the health port only, not the login page). Prometheus is blocked if this stays empty. |
| `dashboard.service.type` | `ClusterIP` |  |
| `dashboard.service.port` | `80` |  |
| `dashboard.ingress` |  | Ingress for the dashboard. Serve it over TLS: passwords and the session cookie cross this connection. With cert-manager, add an issuer annotation and a tls entry, e.g. With dashboard.networkPolicy on, also list the ingress controller in networkPolicy.allowFrom. |
| `dashboard.ingress.enabled` | `false` |  |
| `dashboard.ingress.className` | `""` | IngressClass (cluster default when empty), e.g. nginx, traefik. |
| `dashboard.ingress.host` | `cubeharbor.example.com` |  |
| `dashboard.ingress.annotations` | `{}` |  |
| `dashboard.ingress.tls` | `[]` |  |
| `dashboard.httpRoute` |  | Gateway API HTTPRoute for the dashboard, as an alternative to the Ingress. Needs the Gateway API CRDs (gateway.networking.k8s.io/v1) and a Gateway you run. TLS is terminated by the Gateway's HTTPS listener (with cert-manager: the cert-manager.io/cluster-issuer annotation goes on the Gateway, not here); set cookieSecure: true when it serves HTTPS. |
| `dashboard.httpRoute.enabled` | `false` |  |
| `dashboard.httpRoute.parentRefs` | `[]` | The Gateway (and listener) to attach to. Required, e.g. [{ name: public, namespace: gateway-system, sectionName: https }] |
| `dashboard.httpRoute.hostnames` | `[]` | Hostnames to match, e.g. [cubeharbor.example.com] (empty = every hostname of the listener). Servers can't claim these with `expose: ingress`. |
| `dashboard.httpRoute.annotations` | `{}` |  |
| `dashboard.resources.requests` | `{ cpu: 20m, memory: 32Mi }` |  |
| `dashboard.resources.limits` | `{ memory: 256Mi }` |  |
| `dashboard.nodeSelector` | `{}` |  |
| `dashboard.tolerations` | `[]` |  |
| `dashboard.affinity` | `{}` |  |
| `dashboard.topologySpreadConstraints` | `[]` |  |
| `dashboard.priorityClassName` | `""` |  |
| `dashboard.podLabels` | `{}` | Extra labels and annotations on the dashboard pods (e.g. for log shippers or service meshes). |
| `dashboard.podAnnotations` | `{}` |  |
| `cloudflareTunnel` |  | Reach the dashboard from outside your network through a Cloudflare Tunnel: cloudflared dials out to Cloudflare, so no router port is opened for the dashboard. Only the dashboard (HTTP) goes through it: players still connect to the gateway directly (port forward + DNS, see cloudflareDdns). Put Cloudflare Access in front of the hostname. Setup: README, "Remote access with Cloudflare Tunnel". |
| `cloudflareTunnel.enabled` | `false` |  |
| `cloudflareTunnel.mode` | `token` | `token`: remotely-managed tunnel, public hostnames are configured in the Cloudflare dashboard and the Secret holds the tunnel token. `local`: locally-managed tunnel, the Secret holds the tunnel's credentials.json and the chart renders the ingress rules below. |
| `cloudflareTunnel.existingSecret` | `""` | Existing Secret with the tunnel token (or credentials.json for mode=local). Required: the chart never takes the token as a value. E.g. `kubectl -n cubeharbor create secret generic cubeharbor-cloudflare-tunnel --from-literal=token=<token>`. |
| `cloudflareTunnel.existingSecretKey` | `""` | Key in existingSecret (default `token`, or `credentials.json` for mode=local). |
| `cloudflareTunnel.hostname` | `""` | Public hostname of the dashboard, e.g. dash.example.com. Required for mode=local (it's the ingress rule); with mode=token it's informational. Either way servers can't claim it with `expose: ingress`. |
| `cloudflareTunnel.tunnelId` | `""` | mode=local: the tunnel's UUID (`cloudflared tunnel list`). |
| `cloudflareTunnel.extraIngress` | `[]` | mode=local: more ingress rules, inserted after the dashboard's and before the final 404, e.g. a web map: `- { hostname: map.example.com, service: "http://lobby-map.minecraft.svc:8100" }`. |
| `cloudflareTunnel.originRequest` | `{}` | mode=local: top-level originRequest settings (https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/configure-tunnels/cloudflared-parameters/origin-parameters/). Don't set httpHostHeader for the dashboard: it checks the browser's Origin against Host. |
| `cloudflareTunnel.image` | `cloudflare/cloudflared:2026.10.7` | Pinned for reproducible installs; bump deliberately (https://github.com/cloudflare/cloudflared/releases). |
| `cloudflareTunnel.replicas` | `2` | Each replica holds its own connections to Cloudflare; 2 keeps the dashboard reachable while one restarts. |
| `cloudflareTunnel.pdb` |  | PodDisruptionBudget, created when replicas > 1 so node drains keep one cloudflared up. |
| `cloudflareTunnel.pdb.enabled` | `true` |  |
| `cloudflareTunnel.pdb.minAvailable` | `1` |  |
| `cloudflareTunnel.protocol` | `""` | Edge transport: auto (default, QUIC with HTTP/2 fallback), quic or http2. Use http2 if your network blocks outbound UDP 7844. |
| `cloudflareTunnel.extraArgs` | `[]` | Extra `cloudflared tunnel` flags, e.g. [--loglevel=debug]. |
| `cloudflareTunnel.resources.requests` | `{ cpu: 10m, memory: 32Mi }` |  |
| `cloudflareTunnel.resources.limits` | `{ memory: 128Mi }` |  |
| `cloudflareTunnel.nodeSelector` | `{}` |  |
| `cloudflareTunnel.tolerations` | `[]` |  |
| `cloudflareTunnel.affinity` | `{}` |  |
| `cloudflareTunnel.topologySpreadConstraints` | `[]` |  |
| `cloudflareTunnel.priorityClassName` | `""` |  |
| `cloudflareTunnel.podLabels` | `{}` | Extra labels and annotations on the cloudflared pods (e.g. for log shippers or service meshes). |
| `cloudflareTunnel.podAnnotations` | `{}` |  |
| `tests` |  | `helm test <release>` pods: the dashboard answers /healthz, and the gateway answers a Minecraft status request (mc-router: accepts connections). Nothing runs until you call helm test. |
| `tests.enabled` | `true` |  |
| `tests.image` | `busybox:1.37.0` | Any image with busybox `sh`, `wget`, `nc`, `timeout` and `tr`. Mirror it for air-gapped clusters. |
| `tests.resources.requests` | `{ cpu: 10m, memory: 16Mi }` |  |
| `tests.resources.limits` | `{ memory: 32Mi }` |  |
| `tests.nodeSelector` | `{}` |  |
| `tests.tolerations` | `[]` |  |
| `servers` | `{}` | MinecraftServer resources to create with the release (full spec, see examples/). |
<!-- values:end -->
