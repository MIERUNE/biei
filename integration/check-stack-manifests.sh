#!/usr/bin/env bash
# Cross-service invariants for the composed GKE demo stack.
set -euo pipefail

repo="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
rendered="$(KUBECONFIG=/dev/null kubectl kustomize "$repo")"

kind_count() {
  local kind="$1"
  awk -v wanted="$kind" '$0 == "kind: " wanted { count++ } END { print count + 0 }' \
    <<<"$rendered"
}

expect_count() {
  local kind="$1"
  local wanted="$2"
  local actual
  actual="$(kind_count "$kind")"
  if [[ "$actual" != "$wanted" ]]; then
    printf 'FAIL: expected %s %s resource(s), found %s\n' "$wanted" "$kind" "$actual" >&2
    exit 1
  fi
}

expect_text() {
  local value="$1"
  local description="$2"
  if ! grep -Fq -- "$value" <<<"$rendered"; then
    printf 'FAIL: %s\n' "$description" >&2
    exit 1
  fi
}

expect_count Namespace 1
expect_count Gateway 1
expect_count Deployment 2
expect_count HorizontalPodAutoscaler 2
expect_count HTTPRoute 2
expect_count PodMonitoring 2

expect_text 'name: biei' 'composed stack must include Biei resources'
expect_text 'name: ishikari' 'composed stack must include Ishikari resources'
expect_text 'value: http://ishikari:8080/styles/{style_id}/style.json' \
  'Biei must resolve styles through the in-stack Ishikari Service'
expect_text 'value: http://ishikari:8080/tilesets/{tileset_id}' \
  'Biei must resolve TileJSON through the in-stack Ishikari Service'

printf 'PASS: composed Biei + Ishikari manifests preserve stack contracts\n'
