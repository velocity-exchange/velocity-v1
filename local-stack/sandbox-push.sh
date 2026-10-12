#!/usr/bin/env bash
# Build every pamm-sandbox image for linux/amd64 from this checkout, push it to ECR with your own
# AWS credentials, and print the digest pins for the gitops manifests.
#
#   sandbox-push.sh <vX.Y.Z> [--no-build-programs]
#
# The ECR repos are immutable, so each push needs a new version.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${1:?usage: sandbox-push.sh <vX.Y.Z> [--no-build-programs]}"
[[ "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like v0.1.0" >&2; exit 1; }
REGISTRY="${SANDBOX_REGISTRY:-725652782496.dkr.ecr.eu-west-1.amazonaws.com}"
GIT_SHA="$(git rev-parse HEAD)"
[ -z "$(git status --porcelain --untracked-files=no)" ] || echo "warning: the working tree has uncommitted changes, and the images include them" >&2

source test-scripts/_localnet.sh
checkout_pinned_relay
RELAY_REV="$(git -C "$RELAY_SRC" rev-parse HEAD)"

if [ "${2:-}" != --no-build-programs ]; then
  bash deploy-scripts/build-sbf.sh devnet velocity
  bash deploy-scripts/build-sbf-anchor-v2.sh clob
  bash deploy-scripts/build-sbf-anchor-v2.sh midpoint
  (cd "$RELAY_SRC/programs" && cargo-build-sbf --arch v3 --tools-version v1.57 --manifest-path relay/Cargo.toml)
fi

rm -rf local-stack/programs && mkdir -p local-stack/programs
cp target/deploy/velocity.so anchor-v2/target/deploy/clob.so anchor-v2/target/deploy/midpoint.so \
  "$RELAY_SRC/programs/target/deploy/relay.so" local-stack/programs/

aws ecr get-login-password --region eu-west-1 | docker login --username AWS --password-stdin "$REGISTRY"

METADATA="$(mktemp -d)"
PINS=()

# push <name> <context> <dockerfile> [docker build args...]
push() {
  local name="$1" context="$2" file="$3"; shift 3
  local ref="$REGISTRY/pamm-sandbox/$name:$VERSION"
  echo "== $name =="
  docker buildx build --platform linux/amd64 --push \
    --metadata-file "$METADATA/$name.json" -t "$ref" -f "$file" "$@" "$context"
  PINS+=("$ref@$(sed -n 's/.*"containerimage.digest": *"\([^"]*\)".*/\1/p' "$METADATA/$name.json")")
}

push validator local-stack local-stack/validator.Dockerfile --target sandbox
push tools . docker/ts-app.Dockerfile --target tools --build-arg APP_SCOPE=@velocity-exchange/admin-cli
push relay-turner . docker/relay-turner.Dockerfile --build-arg RELAY_REV="$RELAY_REV"
for app in dlob-server keeper-bots-v2; do
  push "$app" . docker/ts-app.Dockerfile --build-arg APP_PATH="apps/$app" \
    --build-arg APP_SCOPE="@velocity-exchange/$app" --build-arg APP_OUT=lib --build-arg APP_START=lib/index.js
done
for pair in book-publisher:book-publisher swift:swift-server keep-rs:keeprs; do
  push "${pair%%:*}" . docker/rust-app.Dockerfile --build-arg APP_BIN="${pair#*:}" \
    --build-arg GIT_SHA="$GIT_SHA" --build-arg BUILD_VERSION="$VERSION"
done

rm -rf "$METADATA" local-stack/programs
echo
echo "== pins for gitops/non-prod/workloads/pamm-sandbox in infrastructure-v3 =="
printf '%s\n' "${PINS[@]}"
