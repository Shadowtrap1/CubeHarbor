#!/usr/bin/env bash
# Lint and render the chart with the default values, every examples/values-*.yaml and every
# charts/cubeharbor/ci/*-values.yaml, then validate the rendered manifests with kubeconform
# against the oldest and a recent supported Kubernetes version. Also checks that every
# cubeharbor image the chart references is built by docker-bake.hcl, so a release can't publish
# a chart that points at an image nobody pushed.
#
#   scripts/chart-check.sh            needs helm; kubeconform and docker buildx are used when installed
#   KUBE_VERSIONS="1.27.0 1.33.0" scripts/chart-check.sh
set -euo pipefail

cd "$(dirname "$0")/.."
chart=charts/cubeharbor
kube_versions=${KUBE_VERSIONS:-"1.27.0 1.33.0"}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

value_sets=("")
for f in examples/values-*.yaml "$chart"/ci/*-values.yaml; do value_sets+=("$f"); done

i=0
for f in "${value_sets[@]}"; do
  args=()
  [ -n "$f" ] && args=(-f "$f")
  echo "--- ${f:-default values}"
  helm lint --strict "$chart" "${args[@]}" > "$out/lint.log" || { cat "$out/lint.log"; exit 1; }
  for v in $kube_versions; do
    helm template cubeharbor "$chart" -n cubeharbor --kube-version "$v" "${args[@]}" > "$out/$i-$v.yaml"
  done
  i=$((i + 1))
done

if command -v kubeconform > /dev/null; then
  for v in $kube_versions; do
    echo "--- kubeconform, Kubernetes $v"
    # Kinds without a published schema (our MinecraftServer CRD) are skipped; CRDs from other
    # projects (Gateway API, Prometheus Operator) come from the CRDs catalog.
    cat "$out"/*-"$v".yaml | kubeconform -strict -summary -kubernetes-version "$v" \
      -ignore-missing-schemas \
      -schema-location default \
      -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json' \
      -
  done
else
  echo "kubeconform not installed, skipping schema validation"
fi

if docker buildx version > /dev/null 2>&1 && command -v jq > /dev/null; then
  echo "--- images referenced by the chart are built by docker-bake.hcl"
  registry=$(helm show values "$chart" | sed -n 's/^  registry: //p' | head -1)
  tag=$(sed -n 's/^appVersion: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$chart/Chart.yaml")
  REGISTRY=$registry TAG=$tag docker buildx bake --print 2> /dev/null \
    | jq -r '.target[].tags[]' | sort -u > "$out/built.txt"
  {
    # Deployments (operator, gateway, dashboard) with the default values and with mc-router.
    cat "$out"/*.yaml | grep -oE "image: \"?$registry/[^\" ]+" | sed -E 's/^image: "?//'
    # Server pods: the operator appends -java<N> (N from java::AVAILABLE) to SERVER_IMAGE:SERVER_IMAGE_TAG.
    java=$(sed -n 's/^pub const AVAILABLE: .* = \[\(.*\)\];/\1/p' crates/core/src/java.rs | tr -d ',')
    [ -n "$java" ] || { echo "can't read java::AVAILABLE from crates/core/src/java.rs"; exit 1; }
    for j in $java; do echo "$registry/cubeharbor-server:$tag-java$j"; done
  } | sort -u > "$out/used.txt"
  if missing=$(comm -23 "$out/used.txt" "$out/built.txt") && [ -n "$missing" ]; then
    echo "The chart references images that docker-bake.hcl does not build:"; echo "$missing"
    exit 1
  fi
  sed 's/^/  /' "$out/used.txt"
else
  echo "docker buildx or jq not installed, skipping the image check"
fi
echo "chart OK"
