use std::collections::BTreeMap;

use clap::Parser;
use cubeharbor_core::defaults;

/// Operator settings, normally provided by the Helm chart as environment variables.
#[derive(Parser, Debug, Clone)]
#[command(version, about = "cubeharbor Minecraft server operator")]
pub struct Config {
    /// Server image repository; the tag is `<tag>-java<N>`.
    #[arg(long, env = "SERVER_IMAGE", default_value = defaults::SERVER_IMAGE)]
    pub server_image: String,
    #[arg(long, env = "SERVER_IMAGE_TAG", default_value = env!("CARGO_PKG_VERSION"))]
    pub server_image_tag: String,
    #[arg(long, env = "SERVER_IMAGE_PULL_POLICY", default_value = "IfNotPresent")]
    pub image_pull_policy: String,

    /// Servers get `<name>.<base-domain>` unless they set hostnames.
    #[arg(long, env = "BASE_DOMAIN", default_value = "")]
    pub base_domain: String,
    /// Host players use for extra ports (voice chat, Bedrock). Defaults to the base domain.
    #[arg(long, env = "PUBLIC_HOST", default_value = "")]
    pub public_host: String,

    /// Pool of public ports for voice chat, Bedrock and other extra ports.
    #[arg(long, env = "PORT_RANGE", default_value = defaults::PORT_RANGE)]
    pub port_range: String,

    /// Who routes Java connections by hostname: `gateway` (cubeharbor-gateway reads the
    /// MinecraftServer resources) or `mc-router` (itzg/mc-router reads Service annotations).
    #[arg(long, env = "ROUTER", default_value = "gateway", value_parser = ["gateway", "mc-router"])]
    pub router: String,

    #[arg(long, env = "GATEWAY_NAMESPACE", default_value = defaults::NAMESPACE)]
    pub gateway_namespace: String,
    /// Service the operator manages for allocated ports.
    #[arg(long, env = "GATEWAY_PORTS_SERVICE", default_value = "cubeharbor-gateway-ports")]
    pub gateway_ports_service: String,
    /// ConfigMap (in the gateway namespace) the gateway records wake requests in.
    #[arg(long, env = "WAKE_CONFIGMAP", default_value = cubeharbor_core::wake::DEFAULT_CONFIGMAP)]
    pub wake_configmap: String,
    /// Gateway pod labels, `k=v,k=v`.
    #[arg(long, env = "GATEWAY_SELECTOR", default_value = "app.kubernetes.io/component=gateway")]
    pub gateway_selector: String,
    #[arg(long, env = "GATEWAY_SERVICE_TYPE", default_value = "LoadBalancer")]
    pub gateway_service_type: String,
    /// JSON object of annotations for the ports service (e.g. MetalLB sharing keys).
    #[arg(long, env = "GATEWAY_SERVICE_ANNOTATIONS", default_value = "{}")]
    pub gateway_service_annotations: String,
    #[arg(long, env = "GATEWAY_EXTERNAL_TRAFFIC_POLICY", default_value = "Local")]
    pub gateway_external_traffic_policy: String,

    /// Default EULA acceptance for servers that don't set `acceptEula`.
    #[arg(long, env = "ACCEPT_EULA", default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    pub accept_eula: bool,

    #[arg(long, env = "CURSEFORGE_API_KEY", hide_env_values = true)]
    pub curseforge_api_key: Option<String>,

    /// Shared download cache PVC (ReadWriteMany), mounted into servers in `cache_namespace`.
    #[arg(long, env = "CACHE_PVC")]
    pub cache_pvc: Option<String>,
    #[arg(long, env = "CACHE_NAMESPACE", default_value = defaults::NAMESPACE)]
    pub cache_namespace: String,

    #[arg(long, env = "DEFAULT_STORAGE_CLASS")]
    pub default_storage_class: Option<String>,
    #[arg(long, env = "INGRESS_CLASS")]
    pub ingress_class: Option<String>,
    #[arg(long, env = "DOWNLOAD_CONCURRENCY", default_value_t = defaults::DOWNLOAD_CONCURRENCY)]
    pub download_concurrency: u32,

    /// Only manage servers in this namespace (all namespaces when empty).
    #[arg(long, env = "WATCH_NAMESPACE", default_value = "")]
    pub watch_namespace: String,

    /// `/healthz`, `/readyz` and Prometheus `/metrics`.
    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:8080")]
    pub health_addr: String,

    /// Elect a leader with a Lease so several replicas can run; only the leader reconciles.
    #[arg(long, env = "LEADER_ELECTION", default_value_t = false, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub leader_election: bool,
    /// Name of the Lease, in POD_NAMESPACE (or the gateway namespace when that's empty).
    #[arg(long, env = "LEADER_ELECTION_LEASE", default_value = "cubeharbor-operator")]
    pub leader_election_lease: String,
    #[arg(long, env = "POD_NAMESPACE", default_value = "")]
    pub pod_namespace: String,
    /// This replica's identity in the Lease (the pod name; HOSTNAME when empty).
    #[arg(long, env = "POD_NAME", default_value = "")]
    pub pod_name: String,

    /// Allow `spec.image` to replace the cubeharbor server image. Off by default: whoever can
    /// edit a MinecraftServer could otherwise run any container image in its namespace.
    #[arg(long, env = "ALLOW_IMAGE_OVERRIDE", default_value_t = false, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub allow_image_override: bool,
    /// Image prefixes accepted for `spec.image` when overrides are allowed (comma separated;
    /// empty = any image).
    #[arg(long, env = "ALLOWED_IMAGE_PREFIXES", default_value = "")]
    pub allowed_image_prefixes: String,

    /// Hostname suffixes accepted for `expose: ingress` ports (comma separated). Defaults to the
    /// base domain; when both are empty any host is accepted.
    #[arg(long, env = "INGRESS_HOST_SUFFIXES", default_value = "")]
    pub ingress_host_suffixes: String,
    /// Hostnames servers may never claim with an Ingress (e.g. the dashboard's own host).
    #[arg(long, env = "RESERVED_HOSTS", default_value = "")]
    pub reserved_hosts: String,

    /// Create a NetworkPolicy per server that only lets the dashboard reach RCON.
    #[arg(long, env = "RCON_NETWORK_POLICY", default_value_t = true, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub rcon_network_policy: bool,
    /// Namespace and pod labels (`k=v,k=v`) of the dashboard, the only RCON client.
    #[arg(long, env = "DASHBOARD_NAMESPACE", default_value = defaults::NAMESPACE)]
    pub dashboard_namespace: String,
    #[arg(long, env = "DASHBOARD_SELECTOR", default_value = "app.kubernetes.io/component=dashboard")]
    pub dashboard_selector: String,

    /// How many running servers of one MinecraftServerTemplate may restart at once for a
    /// template change; the rest wait until those are ready again (minimum 1).
    #[arg(long, env = "TEMPLATE_ROLLOUT_MAX_UNAVAILABLE", default_value_t = 1)]
    pub template_rollout_max_unavailable: usize,

    /// CPU request for servers that don't set `resources.cpu` ("0" = no request).
    #[arg(long, env = "DEFAULT_CPU_REQUEST", default_value = defaults::CPU_REQUEST)]
    pub default_cpu_request: String,

    /// Pod settings applied to every server pod, as JSON (see `ServerPodDefaults`). Set from
    /// `serverPods` in the Helm chart.
    #[arg(long, env = "SERVER_POD_DEFAULTS", default_value = "{}")]
    pub server_pod_defaults: String,
    /// Parsed `server_pod_defaults` (filled in by `Config::load`).
    #[arg(skip)]
    pub server_pod: ServerPodDefaults,

    /// `ipFamilyPolicy` of the gateway ports Service (`SingleStack`, `PreferDualStack`,
    /// `RequireDualStack`). Cluster default when empty.
    #[arg(long, env = "GATEWAY_IP_FAMILY_POLICY", default_value = "")]
    pub gateway_ip_family_policy: String,
    /// `ipFamilies` of the gateway ports Service, comma separated (`IPv4,IPv6`).
    #[arg(long, env = "GATEWAY_IP_FAMILIES", default_value = "")]
    pub gateway_ip_families: String,

    /// Keep Cloudflare A records pointed at the cluster's public IPv4 address (see `dns.rs`).
    #[arg(long, env = "CLOUDFLARE_DNS", default_value_t = false, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub cloudflare_dns: bool,
    /// Needs Zone > DNS > Edit on the zone.
    #[arg(long, env = "CLOUDFLARE_API_TOKEN", hide_env_values = true)]
    pub cloudflare_api_token: Option<crate::dns::ApiToken>,
    /// Zone name (`example.com`) or zone ID.
    #[arg(long, env = "CLOUDFLARE_ZONE", default_value = "")]
    pub cloudflare_zone: String,
    #[arg(long, env = "CLOUDFLARE_API_URL", default_value = crate::dns::DEFAULT_API_URL)]
    pub cloudflare_api_url: String,
    /// Record names to manage, comma separated. Defaults to `*.<base-domain>`.
    #[arg(long, env = "DNS_RECORDS", default_value = "")]
    pub dns_records: String,
    /// Also publish servers' hostnames that fall inside the zone.
    #[arg(long, env = "DNS_SERVER_HOSTNAMES", default_value_t = false, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub dns_server_hostnames: bool,
    /// How often the public IP is checked (`30s`, `5m`, `1h`).
    #[arg(long, env = "DNS_INTERVAL", default_value = "5m")]
    pub dns_interval: String,
    /// TTL of records cubeharbor writes; 1 = Cloudflare's automatic TTL.
    #[arg(long, env = "DNS_TTL", default_value_t = 1)]
    pub dns_ttl: u32,
    /// URLs that answer with the caller's public IPv4 as plain text, tried in order.
    #[arg(long, env = "PUBLIC_IP_URLS", default_value = crate::dns::DEFAULT_IP_URLS)]
    pub public_ip_urls: String,
}

/// Cluster-specific settings for server pods: private registries, dedicated or tainted game
/// nodes, priority, spreading and platforms with their own UID rules (OpenShift).
#[derive(serde::Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerPodDefaults {
    #[serde(default)]
    pub image_pull_secrets: Vec<serde_json::Value>,
    /// Merged with each server's `nodeSelector` (the server wins on conflicts).
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default)]
    pub tolerations: Vec<serde_json::Value>,
    #[serde(default)]
    pub affinity: Option<serde_json::Value>,
    #[serde(default)]
    pub topology_spread_constraints: Vec<serde_json::Value>,
    #[serde(default)]
    pub priority_class_name: Option<String>,
    #[serde(default)]
    pub runtime_class_name: Option<String>,
    /// Extra pod labels and annotations (cubeharbor's own labels can't be overridden).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Replaces the pod securityContext. Use `{}` on OpenShift so the restricted SCC picks the UID.
    #[serde(default)]
    pub security_context: Option<serde_json::Value>,
}

fn csv(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

fn labels(s: &str) -> BTreeMap<String, String> {
    s.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

impl Config {
    /// Parse flags/env and the JSON settings inside them.
    pub fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::parse();
        cfg.server_pod = serde_json::from_str(&cfg.server_pod_defaults)
            .map_err(|e| anyhow::anyhow!("SERVER_POD_DEFAULTS is not valid: {e}"))?;
        let (lo, hi) = cfg.port_range();
        if cfg.gateway_service_type == "NodePort" && (lo < 30000 || hi > 32767) {
            tracing::warn!(
                "PORT_RANGE {lo}-{hi} is outside the usual NodePort range 30000-32767; allocated ports will fail \
                 unless the API server's --service-node-port-range includes them"
            );
        }
        Ok(cfg)
    }

    pub fn gateway_ip_families(&self) -> Vec<&str> {
        csv(&self.gateway_ip_families).collect()
    }

    pub fn port_range(&self) -> (u16, u16) {
        let (a, b) = self.port_range.split_once('-').unwrap_or((&self.port_range, &self.port_range));
        let a = a.trim().parse().unwrap_or(defaults::PORT_RANGE_FALLBACK_START);
        let b = b.trim().parse().unwrap_or(a);
        (a.min(b), a.max(b))
    }

    pub fn gateway_selector(&self) -> BTreeMap<String, String> {
        labels(&self.gateway_selector)
    }

    pub fn dashboard_selector(&self) -> BTreeMap<String, String> {
        labels(&self.dashboard_selector)
    }

    /// Is `image` acceptable as a `spec.image` override?
    pub fn image_allowed(&self, image: &str) -> Result<(), String> {
        if !self.allow_image_override {
            return Err("spec.image overrides are disabled on this operator (ALLOW_IMAGE_OVERRIDE)".into());
        }
        let mut prefixes = csv(&self.allowed_image_prefixes).peekable();
        if prefixes.peek().is_none() || prefixes.any(|p| image.starts_with(p)) {
            Ok(())
        } else {
            Err(format!("image {image} is not in ALLOWED_IMAGE_PREFIXES"))
        }
    }

    /// May a server expose an Ingress on `host`?
    pub fn ingress_host_allowed(&self, host: &str) -> Result<(), String> {
        let host = cubeharbor_core::crd::normalize_host(host);
        if host.is_empty() || host.contains('*') {
            return Err(format!("invalid ingress host `{host}`"));
        }
        if csv(&self.reserved_hosts).any(|r| cubeharbor_core::crd::normalize_host(r) == host) {
            return Err(format!("ingress host {host} is reserved"));
        }
        let mut suffixes: Vec<String> = csv(&self.ingress_host_suffixes).map(|s| s.trim_matches('.').to_ascii_lowercase()).collect();
        if suffixes.is_empty() {
            suffixes.extend(self.base_domain().map(|d| d.trim_matches('.').to_ascii_lowercase()));
        }
        if suffixes.is_empty() || suffixes.iter().any(|s| host == *s || host.ends_with(&format!(".{s}"))) {
            Ok(())
        } else {
            Err(format!("ingress host {host} is outside the allowed domains ({})", suffixes.join(", ")))
        }
    }

    pub fn public_host(&self) -> Option<&str> {
        [self.public_host.as_str(), self.base_domain.as_str()].into_iter().find(|s| !s.is_empty())
    }

    pub fn base_domain(&self) -> Option<&str> {
        Some(self.base_domain.as_str()).filter(|s| !s.is_empty())
    }

    pub fn mc_router(&self) -> bool {
        self.router == "mc-router"
    }

    pub fn image_for(&self, java: u8) -> String {
        format!("{}:{}-java{java}", self.server_image, self.server_image_tag)
    }
}
