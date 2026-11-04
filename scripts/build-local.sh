#!/bin/sh
# Build, push and (optionally) deploy every cubeharbor image from this machine, without CI.
#
#   REGISTRY=registry.example.com/you scripts/build-local.sh            build + push <registry>/cubeharbor-*:<branch> and :<branch>-<sha>
#   REGISTRY=... scripts/build-local.sh --deploy   ... then helm upgrade the release and restart it
#   REGISTRY=... scripts/build-local.sh --no-push  build only
#
# Environment: REGISTRY (required), TAG (default: branch name), JAVA_VERSIONS, RELEASE,
# NAMESPACE, VALUES (extra Helm values file for --deploy), BUILDER (podman or docker; default:
# whichever is installed). Log in to the registry first. Tests run inside the build
# (`--target test`), so a failing test stops the release.
set -eu

REGISTRY=${REGISTRY:?set REGISTRY, e.g. REGISTRY=ghcr.io/you}
BRANCH=$(git rev-parse --abbrev-ref HEAD | tr '/' '-')
TAG=${TAG:-$BRANCH}
SHA=$(git rev-parse --short HEAD)
JAVA_VERSIONS=${JAVA_VERSIONS:-"8 17 21 25"}
RELEASE=${RELEASE:-cubeharbor}
NAMESPACE=${NAMESPACE:-cubeharbor}
VALUES=${VALUES:-}
BUILDER=${BUILDER:-$(command -v podman >/dev/null 2>&1 && echo podman || echo docker)}

push=true deploy=false
for a in "$@"; do
  case $a in
    --deploy) deploy=true ;;
    --no-push) push=false ;;
    -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

cd "$(dirname "$0")/.."
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  echo "note: uncommitted changes are included, but the image is tagged with HEAD ($SHA)" >&2
fi

images=""
build() { # build <target> <image> <tag suffix> [build args...]
  target=$1 name=$2 suffix=$3; shift 3
  echo "==> $name:$TAG$suffix"
  $BUILDER build --target "$target" "$@" \
    -t "$REGISTRY/$name:$TAG$suffix" -t "$REGISTRY/$name:$TAG-$SHA$suffix" .
  images="$images $REGISTRY/$name:$TAG$suffix $REGISTRY/$name:$TAG-$SHA$suffix"
}

echo "==> tests"
$BUILDER build --target test .
for c in operator gateway dashboard; do build "$c" "cubeharbor-$c" ""; done
for j in $JAVA_VERSIONS; do build server cubeharbor-server "-java$j" --build-arg JAVA_VERSION="$j"; done

if $push; then
  for img in $images; do
    echo "==> push $img"
    $BUILDER push "$img"
  done
fi

if $deploy; then
  echo "==> helm upgrade $RELEASE (tag $TAG)"
  # The branch tag is reused for every build, so always pull it.
  set -- --set image.registry="$REGISTRY" --set image.tag="$TAG" --set image.pullPolicy=Always
  if [ -n "$VALUES" ]; then set -- -f "$VALUES" "$@"; fi
  helm upgrade --install "$RELEASE" charts/cubeharbor -n "$NAMESPACE" --create-namespace "$@"
  # Same tag, new image: pullPolicy Always picks it up on restart.
  kubectl -n "$NAMESPACE" rollout restart deploy -l app.kubernetes.io/instance="$RELEASE"
  kubectl -n "$NAMESPACE" rollout status deploy -l app.kubernetes.io/instance="$RELEASE" --timeout=180s
  echo "Server pods pick up a new cubeharbor-server image on their next restart (dashboard: Restart)."
fi
echo "done: $TAG ($SHA)"
