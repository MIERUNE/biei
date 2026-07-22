# Shared Authentication Decision Queue

Status: **no auth implementation is authorized.** The proposal remains exploratory in [`../specs/auth-sketch.md`](../specs/auth-sketch.md). This file owns unresolved decisions; it is not a roadmap and does not duplicate service-specific work.

## Settled constraints

- Verify locally from an immutable in-memory snapshot; never call object storage, a database, KMS, or a secret manager per request.
- Use object storage as the initial durable registry source, with immutable complete revisions and one conditionally updated head object. Runtime readers never list the prefix or infer latest state from object names.
- Prefer a dedicated registry bucket/container and workload identity per environment. A prefix is organization, not a portable authorization boundary; do not reuse Ishikari's content-reader identity.
- Store metadata, public verification material, high-entropy API-key verifier digests, and secret references—not raw API keys, HMAC masters, cap subkeys, or private signing keys.
- Prefer a conditional-write admin CLI/object writer before considering a public management API. Management uses cloud/workload identity; application credentials never authorize registry mutation.
- Authentication does not replace edge tenant/IP request limits. Origin-local limits cannot govern CDN cache-hit egress.
- Authorize typed actions against parsed logical style/tileset ids before physical namespace/root resolution. For built-in credentials, authentication and all registry-derived policy use one captured revision; external AuthN may use separate immutable verifier state, but AuthZ still uses one policy snapshot.
- Do not create `mmpf-auth`, move Ishikari's `ObjectStoreRegistry`, or add a generic authenticator/storage/rate-limiter hierarchy before two server implementations prove the shared contract.
- If adopted, start with registry tooling and strong entry/expensive-route auth. Capability URLs are a later phase, not the first implementation.

## Distribution and propagation

The object-store head CAS is the single linearization point, so pods never reach consensus *among themselves* about the registry; each independently converges on what the store says. That fixes the propagation mechanism:

- **Baseline: per-pod polling.** Each pod conditional-GETs `head.json` on a bounded interval and refreshes its immutable snapshot. No coordination, no new subsystem. This is the whole mechanism unless evidence forces more.
- **Raft (and any pod-to-pod consensus) is rejected.** It would create a *second* authority to reconcile with object storage, and its stable-quorum requirement fights the deployment model (HPA-autoscaled 2–6 pods on Spot with frequent preemption → constant membership churn, election storms), while coupling auth availability to consensus health — the correlated-outage SPOF the design avoids. Raft earns its keep only when the replicas themselves *are* the durable authority with linearizable writes; here writes are out-of-band and pods are pure readers.
- **Gossip is at most an optional advisory hint, never authoritative.** If polling latency is ever too slow, reuse the existing gossip bus to carry only a monotonic revision number ("revision ≥ N exists, go look"). Each pod still fetches and fully validates the named revision from object storage and rejects any revision below its observed floor, so a stale or hostile hint can at worst trigger a wasted validated fetch — it can never forge or force a downgrade. Polling remains the correctness backstop; add jitter so a shared hint does not stampede the store. Gossip only accelerates *origin-side* snapshot propagation; it does nothing for CDN-cached capability tiles, which still need explicit purge.
- **Read scopes have a loose revocation SLA, so this is enough.** Read-only access to commodity map data has a low cost-of-staleness, and the real abuse ceiling is edge request/egress limits, not registry-propagation speed (see the edge-limit constraint above) — so a revocation SLA of minutes is acceptable and plain polling at a relaxed interval suffices; gossip is likely never needed. This biases the **read tier toward fail-open** on prolonged refresh failure (serve the last-good snapshot with a generous maximum age). Fail-closed is reserved for the private-data strong tier as a conscious per-scope choice (refines "Registry freshness and failure policy" below).

## Adoption gate

Before implementation, explicitly decide that built-in authentication is a product requirement rather than relying only on a customer gateway, `TrustedHeader`, or service mesh. Name the first protected routes and the operator responsible for key issuance, rotation, revocation, and incident response.

**Recommendation:** prove the registry and `StaticApiKeys` behavior on Biei's expensive `static` route and authenticated entry documents first. Do not start with Ishikari capability URLs while the CDN abuse boundary is unresolved.

## Required before registry or strong-auth implementation

1. **Registry freshness and failure policy:** choose refresh interval, maximum last-known-good age, startup behavior without a valid snapshot, and when prolonged refresh failure becomes fail-closed. The interval is the normal revocation-latency SLA.
2. **IAM and secret backend:** choose a versioned external secret backend where symmetric keys are unavoidable; define separate registry-reader, writer, verifier-secret-reader, and issuer identities, rotation overlap, audit ownership, and the dedicated production bucket/container policy. Secret references must pin immutable provider versions retained through the registry rollback window; encrypted key blobs are not stored in registry objects.
3. **Versioned schema and head contract:** finalize bounds for `registry_id`/audience, monotonic revision, digest, key status, verifier sets, grants, Origin policy, and policy revision. Specify create-only revision writes, backend-version CAS for head, whole-candidate validation, and forward-only rollback.
4. **Backend capability matrix:** test which `object_store` backends support create-only puts, version-token head CAS, conditional reads, and overwrite protection. Define how the CLI refuses unsafe multi-writer operation where those guarantees are unavailable; IAM alone does not require callers to use preconditions. Define immutable-revision/secret-version retention and orphan garbage collection without deleting current or rollback material.
5. **API-key contract:** define transport, bounded split identifier/secret syntax, cryptographic-entropy requirement, one-way verifier construction, constant-time comparison, rotation overlap, one-time secret display, log redaction, and error behavior. Do not store recoverable raw API keys merely because the registry is private.
6. **Authorization grammar:** choose exact allow-only action names, service/audience values, resource kinds, and segment-aware exact/subtree selectors. Specify whether glyphs/sprites inherit a named style grant or use explicit kinds. Define the bounded non-secret policy/representation revision used when authorized principals can receive different bytes; never derive grants from bucket names or physical prefixes.
7. **Registry expiry and rollback safety:** define forward-only rollback that preserves intervening revocation tombstones and requires separately audited explicit reactivation. Decide whether head/revisions carry signed `not_before`/`expires_at`, clock-skew bounds, and break-glass behavior. Monotonic numbering protects a running pod from regression but does not give a fresh pod anti-replay; decide whether object-store IAM/audit is sufficient or an external monotonic/transparency anchor is required.
8. **First and second server order:** confirm the initial route set and acceptance tests, then name the second implementation that must exist before shared extraction.

**Sequencing note.** The backend-capability check (#4) is the first concrete task, not a parallel one: a short spike proving `PutMode::Create` conflicts on an existing key and version-conditional head replacement rejects a stale generation on the *actual* production backend (GCS) must pass before committing to the schema or any server code. It converts the foundational CAS assumption into a tested fact; if it fails, the immutable-revision + conditional-head model itself needs rework, so nothing downstream should start first.

## Deferred capability/CDN decisions

These block capability implementation but do not block registry tooling or strong entry-route authentication:

- Exact canonical payload and bounded parser for `version`, audience, `kid`, `key_id`, epoch, and policy revision; MAC length must provide at least 128 effective bits.
- Capability signing topology: per-key epoch subkeys versus a shared epoch key versus asymmetric signatures. A verifier pod that loads many symmetric keys can forge for every loaded key during the live epochs; per-key derivation does not make whole-pod compromise customer-local.
- Web and native-client epoch lengths. Accepting current and previous epochs permits a capability minted near an epoch start to live for almost two epochs.
- CDN cache-key behavior for the full capability URL and `Origin`, including absent/null/non-origin-form handling and 4xx caching.
- CDN-side request/egress controls, usage alerts, and kill switches. Per-pod token buckets protect only origin traffic and vary with replica count.
- Capability-bearing URL retention and redaction in CDN, Gateway, browser/client, support, and analytics logs.
- Emergency revocation semantics across registry refresh, origin snapshots, accepted epoch overlap, CDN expiry, and explicit purge.
- Whether capability rewriting covers third-party provider URLs or only self-hosted resources.
- Private-data policy: which scopes are always `no-store` and remain on the strong-auth path.
- Offline/bulk export contract; do not turn long-lived tile capabilities into an unbounded download API.

## Revisit object storage only with evidence

A database, public issuance API, or push invalidation is deferred. Reconsider only if head-write contention, registry size, audit/query needs, secret fan-out, or the measured revocation SLA cannot be met by immutable revisions, conditional head updates, and periodic snapshot refresh.
