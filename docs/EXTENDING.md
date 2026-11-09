# Extending cubeharbor

Where to make the common changes, and what limits to keep in mind. Start with
[CONTRIBUTING.md](../CONTRIBUTING.md) for the workflow. Before opening a PR, run `cargo fmt`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` and `scripts/chart-check.sh`.

## Add a loader

1. `crates/core/src/crd.rs`: add a `Loader` variant, and handle it in `as_str` and `parse`. If it
   loads plugins, add it to `is_plugin_server`.
2. `crates/installer/src/loaders.rs`: add the install function to the `match` in `install`. It
   returns the `LaunchSpec`. The compiler forces this step.
3. If modpacks can use it, map the pack's name in `crates/core/src/sources/modrinth.rs` and
   `curseforge.rs`. Modrinth mod lookups use `Loader::modrinth_name`, which is `as_str` unless you
   change it.
4. If Geyser supports it, add it to the platform `match` in `crates/operator/src/resources.rs`.
   That `match` has a `_ => None` arm, so the compiler won't remind you.
5. Add it to the new-server form's loader list in `crates/dashboard/src/site.rs`.
6. `make crd`, since the enum is part of the schema.

## Add a CRD field

1. Add it to `MinecraftServerSpec` in `crates/core/src/crd.rs` with
   `#[serde(default, skip_serializing_if = ...)]`. A field that is absent from existing objects
   must leave them unchanged. Compose files accept it automatically, because `spec` is parsed
   into the same type.
2. Decide how it merges. `merge_template` in `crates/core/src/template.rs` won't compile until you
   do. If it has to be per-server, or the gateway or dashboard reads it directly, keep it out of the
   template, as `routing` and `paused` are: those components only ever see the unmerged spec.
3. Use it from `crates/operator/src/resources.rs`. Anything the installer needs goes through
   `InstallPlan` (`crates/core/src/plan.rs`); the plan hash then restarts the pod when it changes.
   If it affects version resolution, add it to the `resolve_hash` tuple in `reconcile.rs`.
4. Prefer `Option<T>` with no default. A `#[serde(default = ...)]` value ends up in the CRD schema, so
   the API server stores it on every object and "unset" is lost for good. That's why
   `merge_template` has to treat `resources.memory: 4Gi` as unset.
5. `make crd` and commit the result (CI compares it). For a new template field, the same command
   also regenerates `minecraftservertemplates.yaml`. Existing installs must re-apply the CRDs, or
   the API server silently drops the new field.
6. A new optional **status** field also goes into `CLEARABLE_STATUS_FIELDS` in `reconcile.rs`, or it
   can never be cleared. A test enforces this.

## Add a dashboard route

1. Add the handler to `crates/dashboard/src/handlers.rs` (HTML, using `views.rs` and maud) or
   `api.rs` (JSON), and the `.route(...)` to `router` in `main.rs`.
2. Check `required_role` in `users.rs`. Unlisted state-changing requests need admin, but **any
   unlisted GET is open to viewers**, so list a read route explicitly if it exposes something
   sensitive. Add a case to its tests.
3. New Kubernetes access means a new rule in `charts/cubeharbor/templates/dashboard.yaml`. Keep it
   narrow: the dashboard's Secret access is limited to named Secrets on purpose.
4. `e2e_tests.rs` covers the router end to end against a fake API server.

## Add a metric

`cubeharbor_core::health::Metrics` builds the Prometheus text (`counter`, `gauge`, `summary`,
`labeled`). Add a field to `Stats` and a line to `render` in `crates/operator/src/metrics.rs`; the
gateway's are in `crates/gateway/src/main.rs`. Prefix names with `cubeharbor_<component>_` and
document them under "Health checks and metrics" in the README. Avoid per-player or per-connection
labels: series counts have to stay bounded. A per-server label is fine up to the limits below.

## Add a chart option

1. Add it to `charts/cubeharbor/values.yaml` with a `# --` comment, and to `values.schema.json`
   (installs fail on unlisted top-level keys).
2. Pass it to the component as an environment variable in its template, and read it in the matching
   clap `Config` (for the operator, `crates/operator/src/config.rs`, with `#[arg(long, env = ...)]`).
   Give it a default there, so the binary still runs outside the chart.
3. `make chart-readme`. Add a `charts/cubeharbor/ci/*-values.yaml` case if it changes what's rendered.
4. Grant RBAC narrowly, and never widen Secret access as a side effect.

## Known scaling limits

The design is comfortable for tens of servers and probably hundreds. None of this has been measured
on a large cluster yet.

| Limit | Why | Way out |
|---|---|---|
| Reconcile cost grows with the square of the server count | Every reconcile lists all servers cluster-wide under `alloc_lock` for port allocation and routing conflicts, and servers requeue every 30-60s | Use the controller's reflector cache, and take the fresh list only when actually allocating |
| One active operator | Port allocation is serialised in one process, so extra replicas are only standbys | Sharding needs allocation moved out of process |
| Player polling happens inside reconcile | Each running server is pinged in its reconcile, so a slow server holds that reconcile | Move it to a separate loop |
| Public ports | One Service holds every allocation; cloud load balancers cap listeners, and the pool is `minecraft.portRange` | Share ports per protocol, or one LB per group |
| Gateway per-port tasks | The gateway binds a forwarder per allocation; UDP flows are tracked per client | Scale out with `gateway.replicas` |
| Dashboard lists on each request | The server list calls the API server every time and has no cache | Add a reflector like the gateway's |
| One pod per server | Worlds are single-writer PVCs, so a server can't scale out | Proxy fleets (see [ROADMAP](ROADMAP.md)) |
| Template fan-out | An edit restarts every affected server within a minute, with no throttle | Staged rollouts (see [ROADMAP](ROADMAP.md)) |
| Template reads | One `get` per templated server per reconcile, no cache | Watch templates once the fleet is large |
