REGISTRY ?= ghcr.io/shadowtrap1
TAG ?= 0.1.0

.PHONY: test lint crd images push chart-lint local local-deploy
test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

# Regenerate the CRD shipped in the chart from the Rust types.
crd:
	cargo run -q -p cubeharbor -- crd > charts/cubeharbor/crds/minecraftservers.yaml

images:
	REGISTRY=$(REGISTRY) TAG=$(TAG) docker buildx bake --load

push:
	REGISTRY=$(REGISTRY) TAG=$(TAG) docker buildx bake --push

chart-lint:
	helm lint charts/cubeharbor
	helm template test charts/cubeharbor > /dev/null

# Build + push every image from this machine with podman or docker ($(REGISTRY)/cubeharbor-*:<branch>).
local:
	REGISTRY=$(REGISTRY) scripts/build-local.sh

# ... and roll it out to the cubeharbor release on this cluster.
local-deploy:
	REGISTRY=$(REGISTRY) scripts/build-local.sh --deploy
