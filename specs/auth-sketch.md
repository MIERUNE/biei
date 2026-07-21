# Access-token protection for biei / ishikari — design sketch

Status: **exploratory design sketch; not adopted or implemented.** Captures the
July 2026 design discussion. Nothing here is an implementation contract, and
implementation should not begin without a separate decision. [`biei-spec.md`](biei-spec.md)
and [`ishikari-spec.md`](ishikari-spec.md) remain authoritative for what exists.

## 1. Requirements and constraints

- Protect the public endpoints of both biei and ishikari with access tokens.
- Request profile is map-shaped, not API-shaped: one page/session issues
  hundreds of tile / glyph / sprite requests, each cheap and latency-sensitive.
  biei `static` renders are the opposite: expensive, low-volume, poorly
  cacheable (arbitrary camera parameters).
- A CDN sits in front, but a *dumb* one: no signed-URL/cookie validation, no
  edge compute. The only lever is the cache-key configuration (full URL, plus
  optionally named request headers).
- The products may be sold as self-hosted middleware: customers already have
  their own auth (API gateways, OIDC/Keycloak, service mesh), so the auth
  boundary must be pluggable, with our own scheme as one implementation.
- Long-lived clients exist (car navigation): `style.json` is fetched once and
  tile URLs are then used for many hours; client firmware may be old and
  cannot be assumed to handle renewal.

## 2. Core decision: verify in-process, issue out-of-band

Built-in credential **verification** happens inside biei/ishikari from one
immutable in-memory registry snapshot: high-entropy API-key verifiers, locally
verified asymmetric session tokens, or compact capability MACs depending on the
tier. External JWT, `TrustedHeader`, or `ForwardAuth` authenticate independently
of that registry, but normalized principals still pass one captured server-local
policy snapshot. There is no request-path object-store, database, KMS, or
secret-manager lookup for built-in schemes. Start with an implementation in the
owning server; extract shared verification primitives only after both servers
have real implementations with
the same contract (§8).
No per-request hop to an external auth service — at tile QPS an extra network
round trip per request dominates latency and creates a single point of failure
in front of everything (the correlated-outage failure mode again).

An out-of-band management plane starts as an admin CLI for static API keys and
registry updates; no service is required for that phase. A later issuer may mint
asymmetric session tokens or capabilities and rotate signing material, but it
remains off the hot path. Hops are spent per issuance/management operation,
never per data request.

Rejected for the request path: ext_authz-style callout services, managed API
gateways (per-request hop, SPOF, cost), IAP (Google identities, wrong fit for
map API keys), LB service extensions (a callout service by another name).

## 3. Token model: two tiers

### 3.1 Entry documents — strong auth, no downstream caching

`style.json`, TileJSON, preview HTML, and biei `static` renders are
authenticated with the real credential (customer API key or session token)
and served `Cache-Control: private/no-store`. These are low-volume, so strong
checks and zero downstream caching are affordable. Shared internal caches may
still hold an unpersonalized provider representation or render result when its
semantic cache identity is independent of the principal (§8.2). `static` is
also the expensive, abuse-attractive endpoint — it gets the strict tier by
design.

### 3.2 Tiles / glyphs / sprites — epoch capability (`cap`)

Map stacks have natural indirection: client → `style.json` → TileJSON → tile
URL templates. When ishikari serves an entry document (authenticated), it
embeds a **capability token** into the tile/glyph/sprite URL templates:

```
/t/{cap}/{z}/{x}/{y}.png
payload = canonical(version, audience, kid, key_id, epoch, policy_revision)
cap = base64url(payload) "." base64url(HMAC(epoch_subkey, payload)[0..16])
```

The concrete encoding remains open, but the authenticated payload is not. It
must be length-bounded and canonically encoded, identify the format version and
verification key (`kid`), bind the deployment audience, customer key, epoch, and
bounded policy revision. The MAC must provide at least 128 effective bits; a
shorter opaque digest without audience, `kid`, epoch, or policy revision would
require unbounded key trials or ambiguous policy lookup during rotation.

- Verified statelessly at origin (pure computation; no hop).
- Cache-sharing unit is **(customer key, epoch)** — all visitors of one
  customer's site share one `cap`, so CDN hit rate within a site is the same
  as with no auth at all. Fragmentation is bounded by the number of keys, not
  users or sessions. (Per-session tokens in the cache key would make every
  session cold; that combination is dead on arrival with a dumb CDN.)
- Origin accepts the current and previous epoch (no thundering herd at
  rotation). A capability minted near the start of an epoch can therefore
  remain usable for almost two epoch lengths, not one. CDN object TTL ≤ epoch
  length bounds cached survival separately; emergency revocation also requires
  a CDN purge policy.

### 3.3 Accepted weaknesses

With a dumb CDN, a cached object is served to anyone presenting a matching
URL. The effective protection of cached content is therefore *capability
possession* for up to the accepted epoch window (almost two epochs when current
and previous are accepted) — acceptable only for commodity base-map tiles.
Capability URLs are bearer credentials and will appear in ordinary CDN,
Gateway, browser-history, and client logs; retention, access, and redaction
policy are part of the security boundary. **Escape hatch:** any tileset carrying
private data is marked `no-store` per scope and always takes the strong-auth
path; this must be a per-scope cache policy from day one.

### 3.4 Verification material and signer isolation

The tiers differ in what verifiers must hold. §2's local-verification decision
is preserved in every case, but the registry should carry the weakest material
that supports the selected credential:

- **Static API keys (§3.1).** Generate a high-entropy random secret and return it
  once. The registry stores a bounded key identifier and a one-way verifier
  digest, not the recoverable key and normally not a secret-manager reference.
  A fast digest is acceptable only because the secret has cryptographic entropy;
  human-chosen passwords would require a different scheme. Exact syntax and
  verifier construction remain open.
- **Entry-document / session tokens (§3.1).** Sign with a rotating private key
  held only by the issuer (KMS/HSM, never exported) and put the corresponding
  bounded public-key set in the registry. A compromised verifier cannot mint
  session tokens. Asymmetric verification cost and signature size are acceptable
  for this low-volume tier.
- **Tile / glyph / sprite `cap` (§3.2).** A short MAC keeps every tile URL and CDN
  key smaller, but any pod that can verify a symmetric MAC also holds material
  capable of minting one. Per-`(key_id, epoch)` subkeys limit time but not a
  whole-pod compromise to one customer: a pod serving the complete registry will
  hold active subkeys for many keys. A shared epoch key reduces registry and
  secret-manager fan-out but has the same broad live-epoch forgery boundary;
  asymmetric capabilities avoid verifier minting but add signature bytes and
  CPU. This topology is unresolved and blocks capability adoption.

Long-lived issuer masters and private signing keys never enter object storage or
verifier pods. If symmetric capability keys are eventually selected, the object
registry contains only version-pinned external secret references and bounded
metadata; verifiers resolve the complete current/previous material set during snapshot
refresh, never per request. Rotation publishes verification material at least
one refresh interval before use. Do not claim a per-customer compromise boundary
unless deployment sharding actually ensures that a pod receives only that
customer's material.

## 4. CDN contract

- Cache key = **full URL (cap included) + the `Origin` request header**,
  configured explicitly in the CDN's cache-key settings. Do not rely on the
  `Vary` response header (many CDNs ignore arbitrary `Vary` values).
- **`Origin`, not `Referer`.** `Origin` is spec-guaranteed to be origin-form
  (scheme+host+port, never a path). A full-URL `Referer` in the cache key
  would (a) silently fragment the cache per page path, (b) leak end-user
  paths/query strings into CDN cache keys and logs, and (c) open an
  auth-passing cache-pollution DoS: `Referer: https://allowed.example/random-N`
  passes the origin check and caches unlimited valid 200 variants. `Origin`
  eliminates all three structurally.
- Guard anyway: if the keyed header value is not origin-form, serve the
  response with `no-store` (default; graceful for misconfigured clients, and
  it neuters cache pollution because such 200s are never stored). A strict
  per-key mode may 403 instead (`origin_not_origin_form`).
- 4xx responses are `no-store` (or minimal TTL) so a transient rejection
  cannot poison a variant.
- Absent `Origin`: browsers omit it on same-origin GETs and non-browser
  clients never send it. Treat `Origin: null` as absent. Per-key policy
  decides whether absent is acceptable (§5). A future authenticated preview
  must use an appropriate key tier for same-origin tile GETs; a referrer policy
  does not manufacture an `Origin` header.
- Origin-local token buckets see cache misses only. They cannot limit requests
  or bytes served as CDN cache hits, so the deployment contract must include
  CDN-side request/egress limits or equivalent alerts and kill switches.
  Expensive entry routes require an auditable edge tenant/IP abuse boundary
  even when origin admission is bounded. Authentication is not that boundary.

## 5. Origin binding (anti-hotlink)

Key registry entries carry `allowed_origins` and an enforcement mode:

| Key tier | Policy |
|---|---|
| Web key | `Origin` required and must match `allowed_origins` |
| SDK / native key (car-nav apps etc.) | absent `Origin` accepted; other controls apply (§6) |

Because the origin value is part of the CDN cache key, the binding holds
**even on cache hits**: a hotlinking site's visitors present a different
`Origin`, miss the cache, reach origin, and are rejected — the legitimate
site's cached variants are unreachable to them. (This closes the classic
"referer checks don't survive CDN caching" hole.)

Honest scope: `Origin` is client-supplied and trivially forged by non-browser
scrapers. This control is **anti-hotlink, not anti-scraper**. Scraping is
controlled by edge request/egress policy plus origin protection (§6), not by
header checks.

## 6. Long-lived clients (car navigation)

Failure mode: clients fetch `style.json` once, so the `cap` is baked in for
the whole session; an epoch rotation mid-drive would 403 tiles while driving.
Old firmware cannot be assumed to renew.

Design response — stop defending freshness, bound *volume* instead:

- **Per-key epoch schedules.** `cap` derivation already includes `key_id`, so
  epoch length can vary per key: web keys ~1 day; SDK/nav keys ~30 days, with
  the previous epoch accepted (≥ one full epoch of residual validity). Long
  epochs also *improve* CDN hit rate for fleets sharing road corridors.
- **Two-layer volume controls.** Local per-pod token buckets cheaply protect
  origin capacity on misses, but HPA changes their aggregate allowance and CDN
  hits bypass them entirely. CDN-side request/egress controls (or at minimum
  usage alerts plus a fast disable/purge path) bound cached delivery. Do not
  claim that a leaked nav cap is limited to one vehicle's fetch rate unless the
  edge enforces that property.
- **Error contract for renewal-capable clients**: 403 bodies distinguish
  `cap_expired` / `cap_invalid` / `origin_forbidden`. SDKs we control re-fetch
  TileJSON on `cap_expired` (one hop per epoch per device).
- **Soft-expiry option**: accept caps up to N epochs old while counting and
  flagging them — expiry as an anomaly signal rather than a hard gate — for
  fleets that cannot renew.
- **Bulk/offline packs are a separate channel.** If the real use case is
  region pre-download, serve it from a dedicated export endpoint with strong
  short-lived auth and no CDN, instead of letting devices crawl the tile API
  for hours.

## 7. Pluggable auth seam (middleware product)

What must be swappable is not "the sidecar" but the **authentication seam**.
Three layers:

```
Layer 1  AuthN (pluggable)   — who/what credential → normalized Principal
Layer 2  AuthZ (always in-app) — Principal.scopes vs parsed style/tileset id
                                 (needs the URL grammar; cannot be externalized)
Layer 3  CDN capability (optional module) — cap minting at entry documents,
         stateless verify; composes with any Layer 1
```

Layer 1 exposes a small server-local interface with a fixed menu of built-ins
(Rust; no dynamic loading). Do not commit to a trait object or shared crate
until a second implementation demonstrates that the boundary is genuinely
common:

| Implementation | Use |
|---|---|
| `None` | default; evaluation/dev, current behavior |
| `TrustedHeader` | *this is "swappable sidecar/gateway" support*: a fronting proxy (Envoy, Kong, oauth2-proxy, …) verifies and passes identity headers |
| `StaticApiKeys` / `HmacCap` | self-contained; our SaaS and small deployments |
| `Jwt { jwks_url }` | OIDC providers, verified locally with cached keys |
| `ForwardAuth { url }` | nginx `auth_request` / Traefik-compatible escape hatch; reintroduces the per-request hop **as the customer's explicit choice** |

`TrustedHeader` requires a documented trust anchor: the proxy strips inbound
identity headers at the edge, and proxy→app traffic is authenticated by a
shared-secret header, mTLS, or internal-only binding. The 403 error contract
(§6) is fixed across all implementations.

Application credentials authorize data-plane actions only. They never authorize
registry writes, key issuance, rollback, or secret access. The initial admin CLI
uses cloud/workload identity and object-store conditional writes; if a public
management API is ever added, it needs a separate strong administrator identity,
audit contract, and CSRF/replay boundary rather than an `admin` content scope.

**Cluster-internal listener paths are exempt from public authentication** and
remain protected by network policy (or a stronger transport identity when one
is introduced). In-cluster biei→ishikari resource fetches are different: if
they use an authenticated public Ishikari route, Biei supplies a dedicated
service credential. Never forward the end user's credential across that hop.

## 8. Refactoring implications

This sketch is also a boundary review, but it is **not** a reason to add auth
scaffolding before an authenticator exists. Preparatory work must improve the
current system independently of this proposed design.

### 8.1 Existing boundaries to preserve

- Cluster deployments assemble separate public and cluster-internal routers and
  have contract tests that keep internal endpoints off the public listener.
  Biei standalone mode serves one listener but composes public content and
  operational/internal route sets separately. Apply authentication only to the
  public content router; do not recover the distinction later by inspecting path
  strings in one combined middleware.
- Biei's render-output cache is keyed from the parsed render request and style
  revision rather than request metadata. Keep credentials, principals, scopes,
  and capability tokens out of that key and out of `InternalTask`, peer wire
  messages, gossip, and simulator artifacts. A bounded, non-secret `RequestId`
  may cross task and peer-wire boundaries for end-to-end correlation, but it
  must never affect authorization, routing, cache identity, or representation
  selection.
- Ishikari uses validated `TilesetId` and `ResourceRoutingKey` values, Biei has
  a `StyleId`, and Ishikari provider routing uses a closed `ProviderRequest`
  that binds resource kind, logical identity, internal endpoint, and placement
  identity. Future authorization must compare scopes with parsed logical domain
  identifiers, not raw URL prefixes or provider URLs. Keep this request
  domain-specific rather than widening it into a generic routing framework.
- Resource URL diagnostics already remove userinfo, the complete query, and
  fragments. Keep that stronger behavior instead of maintaining an
  auth-parameter denylist that can drift when a new token name is introduced.

### 8.2 Cache and response boundary

"Credentials are not cache keys" does not mean that authorization-dependent
representations may share an entry. If two principals can receive different
bytes for the same parsed resource, derive a bounded, non-secret
**representation partition** (for example a policy or tenant revision) and
include that in the relevant cache identity. Never use the raw credential or
capability as the partition. For built-in registry-backed credentials,
authentication, authorization, Origin policy, and representation partitioning
for one request all come from the same captured `Arc<KeyRegistry>` revision; do
not combine a key accepted under one snapshot with grants from a later refresh.
For `TrustedHeader` or external JWT, the identity verifier may have its own
immutable cache, but the normalized principal is authorized entirely against one
captured policy revision.

For Ishikari entry documents, cache the provider representation below auth,
then perform Origin-dependent and capability-dependent URL rewriting on the
response path. Do not place credential- or capability-bearing style JSON,
TileJSON, or preview HTML in a shared in-process response cache. CDN
`private/no-store` does not protect an incorrectly shared origin cache.

### 8.3 Auth-ready boundaries completed before auth

- Keep Ishikari's domain-specific `ProviderRequest` closed over style, glyph,
  and sprite resources. Its logical identity is safe for diagnostics and future
  authorization; its complete provider URL remains an implementation detail for
  fetching, provider-cache identity, and compatibility-preserving placement.
- Keep public/internal router separation covered by production-router contract
  tests as routes move. Biei's standalone public-content subrouter must remain
  independently layerable even though it shares one listener with operational
  routes. No new shared router abstraction is needed.
- Preserve semantic cache-key constructors as the only way to form cache
  identity; when auth arrives, add tests proving request IDs and credentials do
  not partition shared content, while representation partitions do.
- Preserve typed namespace decomposition for scope matching:
  - Ishikari `TilesetId` enforces `flat-id | namespace/id` (≤ 1 `/`) and exposes
    `namespace() -> Option<&str>` / `local_id() -> &str`.
  - Biei `StyleId` remains arbitrary-depth (`a/b/c`) and exposes its first
    segment through `namespace()`. Match finer scopes as prefixes so `a/`,
    `a/b/`, and the full id all resolve without changing deep identifiers such
    as `carto/gl/voyager-gl-style`.
- Biei classifies each public path and validates its `StyleId` before ingress
  admission. Insert future AuthZ at that seam; keep full tile/static/query
  parsing under the admission guard and avoid constructing an `InternalTask`
  before authorization.
- Biei's tile/static/preview response policy remains server-local. Tiles retain
  shared caching, while static renders and preview HTML use
  `Cache-Control: private, no-store`; do not move this HTTP policy into core
  tasks, semantic cache keys, or peer wire values.

### 8.4 Refactors to defer

Do not create `mmpf-auth`, `Principal`, an `Authenticator` trait hierarchy, a
key-registry abstraction, JWT/JWKS machinery, or a generic rate-limiter yet.
They have no production consumer and several open policy questions below can
change their shape. Implement the first selected verifier locally in its server
crate, implement the second against the same behavior, and extract only the
service-independent overlap. This follows the same two-real-consumer rule used
for the other `mmpf-*` crates.

Ishikari's current `get_origin` helper is also not that shared overlap: it
synthesizes a safe base URL for generated documents from `Origin`,
`X-Forwarded-Proto`, and `Host`. Auth Origin validation accepts or rejects a
client identity claim. The inputs look similar, but the contracts and fallback
behavior differ, so combining them would blur a security boundary.

## 9. Implementation notes

- Once implemented by both servers, consider putting proven
  service-independent verification, auth-specific Origin parsing, and the
  error contract in a small shared crate (for example `crates/mmpf-auth`). Keep
  service-specific URL rewriting and authorization policy in the owning
  server.
- **biei**: run AuthN before public-path parsing and before
  `acquire_admission` (header/query inspection only — no renderer dependency,
  so unlike the degraded gate it belongs at the very front). Then use the
  lightweight parsed `StyleId` for AuthZ before admission; full render/query
  parsing and task construction follow only after authorization and admission.
  The render-output-cache key **must not** include tokens or caps; add a
  non-secret representation partition only if auth policy changes rendered
  bytes. Preview passes the entry credential only to the protected
  style/TileJSON request; derived resource URLs carry the capability instead.
  `redacted_url` removes the entire query from logs.
- **ishikari**: apply one axum layer only to public routes, authorize against the
  parsed logical `TilesetId` or provider resource identity, and keep capability
  embedding in the style.json / TileJSON response path above the shared
  provider cache.
- **Key registry**: deployment `registry_id`/audience, monotonic registry
  revision, `key_id`, credential verifier(s), `kid`, optional secret/subkey
  reference, explicit status, logical action/resource grants, `allowed_origins`,
  tier (web/SDK), epoch schedule, rate tier, and policy revision. Rotation accepts
  a bounded current/previous key set. Revocation reaches origins only after their
  next successful registry refresh; already-cached tiles additionally require
  expiry or purge. Tokens and capabilities bind the deployment audience so a
  credential copied between environments is rejected even if identifiers match.
- **Metering**: origin counters undercount by design (CDN hits never arrive);
  true usage requires CDN log post-processing, on a separate async path.
  Prometheus labels per `key_id` are bounded (registered keys only); reject
  reasons and nonconforming-origin counts get counters so misconfigured
  customers are visible ("your traffic is uncacheable — check client Origin
  handling and the CDN cache-key configuration").

### 9.1 Registry storage: object storage first

The registry is a small object-storage **control plane**, never a data-plane
lookup. Each process is configured with one fixed registry-root URL, resolves it
once at composition time, and refreshes an immutable `Arc<KeyRegistry>` snapshot.
Normal requests perform no object-store, database, KMS, secret-manager, list, or
metadata operation.

Distribution is per-pod polling of the conditional head; the head CAS is the sole
linearization point, so pods never run pod-to-pod consensus. Raft is rejected and
gossip is at most an optional advisory revision hint (never authoritative). Read
scopes have a loose revocation SLA, which biases the read tier toward fail-open on
prolonged refresh failure. See [`../issues/auth-todo.md`](../issues/auth-todo.md)
"Distribution and propagation".

#### 9.1.1 Relationship to existing object-store conventions

Ishikari currently separates a backend's scheme+authority (bucket/host) from the
object path, reuses one store client per authority, and maps a configured
matching logical namespace to its physical root by stripping the first segment;
a default root instead preserves the complete logical key. Preserve the useful
mechanism but not the domain coupling:

- The auth loader may follow the same **configured root → reused store + base
  path** pattern, with ambient workload credentials supplied at the server/CLI
  composition root. Do not make auth depend on Ishikari's read-oriented
  `ObjectStoreRegistry`, move that type out of `ishikari-core`, or reuse
  `NamespacedEntries` before two real auth consumers establish shared code.
- Auth has exactly one registry root per process/environment, not
  `namespace=url;default=url` routing. A registry document cannot name another
  bucket, absolute URL, or path outside that root.
- Logical resource namespaces are authorization input; object-store buckets and
  prefixes are storage placement only. For example, authorizing logical
  `regional/streets` is checked before Ishikari strips `regional/` and resolves
  `{regional-root}/streets.pmtiles`. Possession of a bucket prefix never grants a
  logical scope, and a default physical root never creates a default grant.
- Production registry backends must provide authenticated reads and conditional
  writes (`gs://`, `s3://`, or an equivalent deployment adapter). `file://` and
  `memory://` are development/test options. Arbitrary HTTP(S), especially the
  content loader's plain-HTTP escape hatch, is not an acceptable production auth
  registry.

This is a reason to reuse the `object_store` crate and URL/path discipline, not a
reason to create a generic repository-wide storage framework.

#### 9.1.2 Immutable revisions plus a conditional head

Use a fixed layout below the configured root:

```text
v1/head.json
v1/revisions/{monotonic_revision}-{sha256}.json
```

`head.json` is the only mutable object. It contains bounded `schema_version`,
`registry_id`/audience, monotonic revision, relative revision-object name,
content digest, and creation metadata. A revision is a complete immutable
registry snapshot. The object name and digest make accidental replacement or a
wrong head target detectable; the relative path is validated to remain under
`v1/revisions/`.

The admin writer uses a create-then-publish protocol whose **publication point**
is one compare-and-swap of head; the two object operations are not a transaction:

1. read and validate the current head plus its backend version/ETag/generation;
2. build and fully validate the next complete revision locally;
3. create the revision object with create-only semantics;
4. conditionally replace `head.json` using the observed backend version;
5. report a conflict instead of merging or using last-write-wins.

A failed head CAS may leave an unreachable immutable revision; that is harmless
and can be garbage-collected after a retention window. Rollback never points
head to a lower revision. It creates a **new higher revision** from historical
content, but forward numbering alone is not revocation safety: the planner must
preserve every disable/revocation tombstone introduced after the selected
revision and require an explicit separately audited reactivation to remove one.
A running pod rejects a lower revision than it has observed. A fresh pod has no
trusted revision floor, so defending against malicious bucket rollback requires
an external monotonic/transparency anchor or bounded signed expiry; object-store
IAM and audit are the initial trust boundary, not a claim of cryptographic
anti-replay. Runtime readers never list the prefix or infer the newest revision
from object names.

Pods conditional-GET `head.json`; when unchanged they do no second read. On a
new head they fetch the named immutable revision, verify its digest, bounds,
audience, monotonicity, and every referenced verifier/secret, construct the
complete candidate snapshot, and then perform one `Arc` swap. Every secret
reference is pinned to an immutable provider version (never a mutable `latest`
alias), and that version is retained for at least the registry revision and
rollback-retention window. There is no partially applied manifest: an invalid
entry or unavailable required secret
rejects the whole candidate and retains the last-good snapshot. Revocation is an
explicit valid key status, not a malformed entry. Startup behavior and maximum
last-good age remain policy decisions in [`../issues/auth-todo.md`](../issues/auth-todo.md).

#### 9.1.3 IAM and secret boundary

Prefer a dedicated auth-registry bucket/container per environment over the demo
content bucket. A path prefix is organization, not a portable IAM or encryption
boundary; sharing a bucket can accidentally let a content reader enumerate auth
policy or let a content publisher alter it.

- Server identities get read-only access to `head.json` and revision objects.
- The admin writer gets object create/update permissions but no data-plane
  service credential. Create-only revision writes and conditional head updates
  are enforced by writer behavior plus verified backend capabilities; ordinary
  bucket IAM does not generally force callers to send `If-Match`. Provider
  versioning/Object Lock is optional defense in depth. Cloud audit logs identify
  the real writer; a self-reported `created_by` field is informational only.
- Issuer/KMS identities are separate from registry readers and writers. Raw API
  keys, capability keys, HMAC masters, and private signing keys never appear in
  object bytes. Public keys and high-entropy API-key verifier digests may appear
  inline. If symmetric capabilities are adopted, verifier pods receive a
  separate least-privilege secret-read role limited to the pinned
  current/previous key versions; that role cannot mint through the issuer or
  read the master.
- Biei and Ishikari use separate workload identities even when both can read the
  registry. Do not reuse Ishikari's broad content-bucket reader merely because
  its object-store client already exists.

#### 9.1.4 Authorization and management integration

Authorization grants are typed logical policy, not object paths or free-form URL
prefixes. The schema should represent an allow-only set of:

- service/audience (`biei`, `ishikari`, or an explicitly reviewed shared value);
- action (for example entry read, tile read, tile render, static render);
- resource kind (`style`, `tileset`, or an explicit glyph/sprite policy);
- exact id or segment-aware subtree selector; glyphs/sprites must either inherit
  from a named style grant by a deterministic rule or have their own kind—never
  fall through because they lack a tileset id;
- bounded policy/representation revision where bytes can differ by principal.

Segment-aware matching prevents a grant for `carto` from matching `cartography`.
It also accommodates Biei's arbitrary-depth `StyleId` and Ishikari's
`namespace/id` `TilesetId` without treating their physical source roots as the
same security domain.

A request using a built-in credential captures one registry snapshot,
authenticates the key, authorizes the parsed logical id and action, validates
Origin policy, and derives any representation partition from that same revision
before expensive parsing, admission, or storage resolution. External AuthN may
use its own immutable verifier cache, but the resulting normalized principal
still uses exactly one policy snapshot.

Registry management is a separate control-plane authorization system. The first
admin CLI relies on cloud/workload identity, conditional object writes, and
provider audit logs; application API keys and content grants cannot call it. A
future management API, if justified, must serialize the same CAS workflow and
add a distinct administrator identity and audit/replay contract rather than
introducing an `admin` data-plane scope.

Object storage backs only our built-in key/capability policy. `TrustedHeader` and
external JWT deployments bring their own identity source, though normalized
principals must still pass the same server-local logical AuthZ boundary.

### 9.2 Staged adoption if this sketch is approved

Approval should authorize one bounded phase at a time:

1. Define the versioned registry schema and build an admin CLI/object writer for
   the immutable-revision + conditional-head workflow in §9.1. Include local
   validation, conflict-safe apply, audit output, and forward-only rollback.
   Prefer this narrow operator tool over a public management API.
2. Implement strong `StaticApiKeys` protection for Biei's expensive `static`
   route and authenticated entry documents. Keep edge rate limiting as a
   separate deployment prerequisite.
3. Implement the same externally observable auth behavior in the second server;
   only then extract proven service-independent verification code.
4. Add capability URLs only after the exact wire format, scope partition,
   epoch lifetime, CDN cache-key/log policy, edge abuse controls, and emergency
   purge semantics are settled and tested.

This ordering deliberately does **not** start with Ishikari capability auth: the
registry and strong entry tier are useful without accepting the capability/CDN
trade-offs.

## 10. Decision queue

Unresolved choices and adoption gates are tracked once in
[`../issues/auth-todo.md`](../issues/auth-todo.md). Their presence does not
change this document's exploratory status or authorize implementation.
