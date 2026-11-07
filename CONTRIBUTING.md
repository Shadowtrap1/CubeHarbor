# Contributing

Thanks for helping! Bug reports, docs fixes and pull requests are all welcome.

## Getting started

You need a recent stable Rust toolchain, Helm 3 and (for images) Docker or Podman.

```sh
cargo test --workspace          # unit tests
cargo clippy --workspace --all-targets
make chart-lint                 # helm lint + template
```

Layout: `crates/core` holds the shared types (CRD, compose parser, protocol, upstream clients).
The binaries are `operator`, `gateway`, `dashboard`, `installer`, `runner` and `cubeharbor`. The
Helm chart is in `charts/cubeharbor`. The README's [Components](README.md#components) table
explains what each one does.

## Making changes

* **CRD changes:** if you change the `MinecraftServer` types in `crates/core/src/crd.rs` (or a
  default in `crates/core/src/defaults.rs`), run `make crd` and commit the regenerated
  `charts/cubeharbor/crds/minecraftservers.yaml`. CI fails when it's out of date.
* **Dashboard look:** the stylesheet and scripts are in `crates/dashboard/assets/`. Options
  users can set are in `crates/dashboard/src/site.rs`; document new ones in `examples/site.yaml`.
* **Chart values:** document new values in `charts/cubeharbor/values.yaml` with a `# --` comment,
  and add them to `charts/cubeharbor/values.schema.json` (new top-level keys must be listed there,
  or installs fail). Then run `make chart-readme` to update the values table in
  `charts/cubeharbor/README.md` (CI checks it).
* Add or update tests for behaviour changes. Most of the logic (compose parsing, port
  allocation, protocol, installer) is unit-testable without a cluster.
* Add a line to `CHANGELOG.md` under "Unreleased".

## Pull requests

Keep PRs focused on one change, describe what you tested, and make sure CI passes. Opening a PR
means you agree to license your contribution under the [MIT License](LICENSE).

## Reporting bugs

Open an issue with the cubeharbor version, Kubernetes distribution, the relevant
`MinecraftServer` or compose snippet, and logs (`kubectl logs` of the operator, and
`kubectl logs <server>-0 -c install` for install problems). Please remove passwords and API keys.
Security issues go through [SECURITY.md](SECURITY.md), not public issues.
