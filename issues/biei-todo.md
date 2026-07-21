# Biei Decision Queue

No Biei-specific implementation item is active. These entries are product or operational triggers, not a roadmap. Durable behavior belongs in [`../specs/biei-spec.md`](../specs/biei-spec.md), cross-cutting work belongs in [`refactor.md`](refactor.md), and missing upstream bindings belong in [`mln-rs-wishlist.md`](mln-rs-wishlist.md). Delete resolved entries; git history is the archive.

## Compatibility and product triggers

- **URL markers or text-layer pin labels:** add only for a concrete compatibility requirement. Current labels are intentionally rendered into request-local bitmaps.
- **Public ETag/304 handling:** add only if CDN or gateway validators are insufficient for measured traffic.
- **Standard throughput fixture:** choose a local, fast provider fixture before publishing reproducible throughput comparisons.

## Production security gates

- **Private-network resource authorities:** before accepting attacker-controlled style/resource URLs in a deployment that enables `BIEI_MLN_RESOURCE_PRIVATE_HOSTS`, replace host-only exceptions with exact allowed `(scheme, host, port)` authorities (or an equally narrow structured policy). The current exception permits every HTTP(S) port on an allowed private host; broad wildcard hosts expand that SSRF capability further.

## Operational evidence gates

- **Persistent FileSource cache:** require restart measurements showing enough benefit to justify disk state and invalidation policy.
- **Native subprocess isolation:** require evidence that process-level recovery is insufficient for observed MapLibre Native crashes.
- **Per-render FileSource context:** require a real diagnostic question that aggregate resource metrics, cancellation, and global timeouts cannot answer.
- **Per-peer gossip-age metric:** require an incident that existing membership and readiness signals cannot diagnose.
- **Cold style JSON parsing:** optimize only if setup profiles show material CPU or latency cost.
- **Orphan render memory accounting:** add byte-level orphan admission only if a slow-render or distinct-key measurement shows the count-bounded orphan pool can approach the pod memory limit. Orphans are bounded by count, not bytes (see biei-spec §8.2).
- **Production packaging:** add Helm or broader policy only if Biei moves beyond the current deployment-demo scope.
