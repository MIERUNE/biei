# Biei Decision Queue

No Biei-specific implementation item is currently active. This queue records
bounded planned work and evidence-gated product or operational triggers; it is
not an ordered roadmap. Durable behavior belongs in
[`../specs/biei-spec.md`](../specs/biei-spec.md), cross-cutting work belongs in
[`refactor.md`](refactor.md), and missing upstream bindings belong in
[`mln-rs-wishlist.md`](mln-rs-wishlist.md). Delete resolved entries; git history
is the archive.

## Compatibility and product triggers

- **Object-storage custom marker images (planned):** support managed marker
  images without accepting arbitrary request-supplied URLs. The request carries
  a bounded logical marker ID; deployment configuration resolves that ID through
  an object-storage root/template. Prefer a content-addressed immutable ID and
  object so decoded images and rendered outputs can be cached without an
  invalidation protocol. Before implementation:
  - choose the public overlay syntax and object layout; exact Mapbox `url-*`
    compatibility is not a goal if it would expose an arbitrary fetch target;
  - decide whether Ishikari owns object-store access and serves the bounded
    asset to Biei (preferred, so Biei does not acquire content-store
    credentials) or whether a reusable provider abstraction justifies direct
    access;
  - reject path traversal and request-controlled schemes, authorities, query
    strings, or object-store options after template expansion;
  - define encoded-byte, decoded-dimension, total-pixel, format, and decode-time
    limits; do not enable SVG or other active/externally referencing formats;
  - reuse bounded cache and single-flight behavior, and include marker-fetch
    waiting in resource-I/O metrics and the render deadline;
  - include marker identity and rendering parameters in render-output cache
    keys while keeping coordinates and overlay ordering unchanged; and
  - test cold/warm fetches, duplicate marker reuse, missing/corrupt/oversized
    objects, scale behavior, auto-fit/anchor behavior, and mixed overlay z-order.
- **Text-layer pin labels:** add only for a concrete compatibility requirement.
  Current labels are intentionally rendered into request-local bitmaps.
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
