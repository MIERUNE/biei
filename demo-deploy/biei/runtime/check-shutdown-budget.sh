#!/usr/bin/env bash
# Keep the rendered GKE termination window and Biei's code-owned deadline aligned.
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH='' cd -- "$root/../../.." && pwd)"
rendered="$(KUBECONFIG=/dev/null kubectl kustomize "$root/k8s/overlays/gke")"
deployment="$(printf '%s\n' "$rendered" | awk '
  BEGIN { RS = "---" }
  /kind: Deployment/ && /name: biei/ { print; found = 1; exit }
  END { if (!found) exit 1 }
')"

termination_grace="$(printf '%s\n' "$deployment" | awk '/terminationGracePeriodSeconds:/ { print $2; exit }')"
pre_stop="$(printf '%s\n' "$deployment" | sed -n 's/.*sleep \([0-9][0-9]*\).*/\1/p' | sed -n '1p')"
application_budget="$(sed -n 's/^const PROCESS_SHUTDOWN_BUDGET: Duration = Duration::from_secs(\([0-9][0-9]*\));$/\1/p' "$repo_root/servers/biei/src/runtime/run.rs")"
process_overhead_reserve=1

if [[ -z "$termination_grace" || -z "$pre_stop" || -z "$application_budget" ]]; then
  printf 'FAIL: could not resolve Biei shutdown budget inputs\n' >&2
  exit 1
fi

if [[ "$termination_grace" -ne 25 ]]; then
  printf 'FAIL: GKE Spot termination grace must remain 25s (got %ss)\n' "$termination_grace" >&2
  exit 1
fi

required=$((pre_stop + application_budget + process_overhead_reserve))
if [[ "$required" -gt "$termination_grace" ]]; then
  printf 'FAIL: Biei shutdown budget needs %ss (%ss preStop + %ss app + %ss reserve), exceeding %ss\n' \
    "$required" "$pre_stop" "$application_budget" "$process_overhead_reserve" "$termination_grace" >&2
  exit 1
fi

printf 'PASS: Biei shutdown budget fits %ss (%ss preStop + %ss app + %ss reserve)\n' \
  "$termination_grace" "$pre_stop" "$application_budget" "$process_overhead_reserve"
