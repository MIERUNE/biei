# Authentication boundaries for Biei and Ishikari — design sketch

Status: exploratory. This document describes design boundaries, not an
implementation commitment. Open decisions are tracked in
[`issues/auth-todo.md`](../issues/auth-todo.md).

## 1. Purpose

Authentication serves different purposes on different surfaces. Treating every
request as if it were an administrative operation would add cost and complexity
without materially improving the ordinary delivery path.

For public map delivery, the main goals are:

- attribute usage to a customer or project;
- assign a rate and egress budget;
- block disabled or obviously abusive credentials; and
- keep expensive origin work away from unauthenticated traffic.

This is not a confidentiality boundary for highly sensitive data. A delivery
key is a bearer credential that may be present in a browser or other
comparatively exposed client. It should be cheap to rotate and have limited
authority, but it should not be mistaken for an end-user identity.

Administrative changes, automated publishing, service-to-service calls, and
ordinary map delivery therefore use separate credentials.

Two distinctions are fundamental:

- Authentication assigns a request to a principal or rate bucket. Rate limits,
  request limits, and egress limits enforce the abuse budget.
- Origin metrics measure origin work. They do not measure all delivered usage
  when a CDN can answer requests without contacting the origin.

## 2. Four independent credential planes

| Plane | Principal | Preferred credential | Main authority |
| --- | --- | --- | --- |
| Human administration | employee or operator | one or more corporate OIDC providers | manage configuration, content, and delivery credentials |
| Automated publishing | workload or service account | workload identity or narrowly scoped service credential | publish and update approved content |
| Map delivery | customer or project | high-entropy opaque delivery key | read ordinary delivery routes within a coarse scope |
| Internal service | Biei or another trusted workload | dedicated service identity | call the required Ishikari internal/read APIs |

These planes are deliberately not interchangeable:

- a delivery key cannot authorize a management or publishing route;
- a browser's delivery key is never forwarded as Biei's identity to Ishikari;
- a human OIDC session is not stored in a style URL or used as a long-lived
  service credential; and
- publishing automation does not impersonate a human administrator.

The issuers, audiences, storage, logging, rotation, and failure policies may
therefore differ. Sharing one token format across all four planes would be a
design regression even if the claims could technically express every role.

Supporting several providers does not mean trying every verifier in sequence.
Each credential plane has explicit selection rules and produces one canonical
principal. A recognized but invalid credential must not fall through to a
weaker mechanism.

## 3. Hot-path rule

Ordinary delivery authentication must be local and bounded:

1. Parse a small credential identifier.
2. Perform an O(1) lookup in an immutable in-process snapshot.
3. Verify the secret in bounded time.
4. Return a compact, typed delivery principal to authorization and metering.

The delivery request path must not perform a database, object-store, IdP, KMS,
forward-auth, or other network call merely to authenticate one request. Registry
or secret refresh happens out of band and atomically replaces the local
snapshot.

Authentication should run before admission to expensive work:

- Biei verifies the caller before render admission or native rendering;
- Ishikari verifies the caller before remote storage, peer routing, or derived
  processing; and
- an output or resource cache hit still requires authentication when the route
  is protected, but authentication itself must be cheap enough that this does
  not erase the cache benefit.

Authorization is evaluated against the already parsed route identity. A route
must not be parsed independently by authentication, authorization, caching, and
metrics because disagreement between those parsers can create bypasses.

## 4. Delivery credentials

### 4.1 Token shape

The initial built-in credential can be an opaque key with a public lookup part
and a random secret, for example:

```text
<token_id>.<random_secret>
```

`token_id` selects one bounded registry entry; it grants no authority by
itself. `random_secret` must contain enough entropy to resist online and
offline guessing. Because the secret is machine-generated and high entropy, a
fast keyed digest or cryptographic hash with constant-time comparison is
appropriate. A password-hardening function such as Argon2 on every tile request
would spend CPU without compensating for weak human passwords.

The preferred transport is the standard `Authorization: Bearer` header, but it
can be the deployment default only after the supported browser and MapLibre
clients are proven to attach it to every required style subresource. Putting a
stable delivery key in a query string makes it more likely to appear in browser
history, referrers, screenshots, CDN logs, and support transcripts. A client
that cannot set the header therefore needs an explicit alternative design; it
must not silently fall back to putting the long-lived key in every URL.

### 4.2 Registry entry

A delivery-key entry should contain only bounded policy needed on the hot path:

- token identifier and secret verifier;
- stable customer or project identifier;
- enabled or disabled state;
- rate/egress tier;
- optional coarse product or namespace scope; and
- optional validity bounds used for rotation overlap.

It should not grow into an end-user directory, fine-grained RBAC system, or
arbitrary policy language. If authorization requires remote relationships or
unbounded claim sets, it no longer satisfies the delivery-path cost model.

Raw tokens, secret verifiers, and attacker-controlled identifiers must never be
used as metric labels or emitted to ordinary logs. Logs may contain a bounded
internal token ID when operationally necessary, but a stable customer/project
ID is normally the more useful attribution key.

### 4.3 Scope and revocation

The ordinary delivery tier favors low cost over instant global revocation.
Rotation may allow an old and a new key to overlap, and a disabled key may
remain accepted for the bounded registry-refresh interval. A target measured in
minutes is acceptable for commodity reads if it is documented and monitored.

Invalid, unknown, malformed, or disabled credentials fail closed. Refresh
failure is different:

- a running process may continue using its last known-good snapshot according
  to the tier's explicit, observable staleness policy; and
- a fresh process with no valid snapshot must not silently allow protected
  traffic. It should remain unready or reject protected requests.

This preserves availability during a control-plane interruption without
turning missing configuration into anonymous access.

Data that requires immediate revocation or confidentiality belongs to a
separate strong-access tier described in section 9.

### 4.4 Deliberate compromises and hard boundaries

The ordinary delivery credential is best understood as a project-level abuse
and attribution credential, not as proof of an individual end user. Its design
deliberately accepts that:

- a browser-visible bearer credential can be copied;
- authorization scopes are coarse rather than per-user or per-object;
- revocation may take minutes to reach every edge and origin process;
- shared representations remain shared across authorized customers;
- delivery accounting may be eventually consistent; and
- some clients may eventually require short-lived URL capabilities instead of
  an authorization header.

Those compromises keep commodity map delivery cacheable, portable, and cheap.
They do not relax the following boundaries:

- a delivery credential never authorizes management or publishing;
- secrets are high entropy, rotatable, and absent from ordinary logs and
  metrics;
- unauthenticated traffic cannot consume expensive origin work on a protected
  route;
- edge request and egress limits contain the value of a copied credential; and
- confidential or individually authorized data uses the separate strong-access
  tier rather than stretching the delivery credential beyond its threat model.

If a product cannot tolerate the accepted compromises, it is not an ordinary
delivery-tier product. Changing that product's tier is safer than incrementally
adding management-grade machinery to every tile and glyph request.

## 5. Abuse control and accounting

Authentication identifies the bucket; it does not enforce the budget by
itself. When the edge or gateway supports it, it should apply request-rate and
egress limits using the verified customer/project and rate tier. Expensive Biei
routes may also have stricter concurrency or cost budgets than small Ishikari
resource reads.

A basic CDN may offer no customer-aware enforcement at all. In that case,
origin-local limits still protect origin capacity but cannot constrain traffic
served from CDN cache. The deployment must either accept that limitation, add a
separate authenticating gateway, use coarse provider-level controls, or choose a
URL-bearer model whose exposure is bounded by expiry and cache lifetime. The
core design must not pretend that origin rate limiting governs CDN-hit egress.

Useful bounded dimensions include:

- customer or project;
- rate tier;
- route class;
- authentication outcome; and
- coarse authorization outcome.

Do not put raw token IDs, URLs, tileset IDs, style IDs, or other unbounded
request values in Prometheus labels. Detailed per-resource attribution belongs
in sampled or structured logs, not in a high-cardinality time series. Even a
registry-defined customer/project label needs a documented cardinality budget;
larger fleets should aggregate it outside Prometheus.

When a CDN serves a cache hit, the origin sees neither the request nor its
bytes. Therefore, where the chosen CDN exposes adequate logs:

- CDN or edge access logs are the source of truth for delivered request and
  egress usage;
- origin metrics describe cache misses, origin latency, provider I/O, render
  work, failures, and capacity; and
- origin request counts must not be presented as billing-complete delivery
  counts.

Per-customer attribution is available from those logs only if the edge has
validated the credential and records a bounded derived identity. A raw bearer
token in a CDN log is a credential leak, while an anonymous shared-cache log can
support aggregate accounting but cannot retroactively identify the customer.

If the CDN exposes neither identity-enriched logs nor another trustworthy usage
export, exact per-customer delivery accounting is not available. This is a
deployment capability gap, not something the origin can reconstruct later.

The aggregation pipeline may lag. Enforcement should not depend on a billing
export being immediately available.

## 6. Cache and CDN contract

### 6.1 Portability baseline

The baseline CDN contract assumes only conventional caching by URL under
standard HTTP cache directives, plus whatever aggregate logging the provider
normally supplies. It does not require:

- programmable edge compute;
- an edge KV or replicated credential registry;
- custom token validation or HMAC code;
- customer-aware rate limiting;
- arbitrary cache-key rewriting;
- identity-enriched access logs; or
- immediate, globally consistent purge.

Cloud CDN, CloudFront, Akamai, or another provider may offer some or all of
these features, but the core protocol cannot depend on them. Provider-specific
adapters may enable better authentication, cache sharing, enforcement, and
accounting when available.

A deployment must declare the CDN capabilities it relies on. Configuration
should fail validation when a selected security or accounting policy requires a
capability the deployment does not have; silently degrading from authenticated
delivery to public delivery is not acceptable.

### 6.2 Authentication and shared caching

Authentication must not unnecessarily destroy shared-cache efficiency. When
two authorized callers receive identical representation bytes, the semantic
origin cache key should not contain the caller, raw credential, or OIDC subject.
Authorization happens before returning the cached representation; the stored
representation remains principal-independent.

A CDN creates an additional boundary. A CDN that cannot authenticate a request
cannot simultaneously:

1. serve a shared cache hit without reaching the origin; and
2. guarantee that only authenticated callers receive that hit.

The deployment must choose and document one of these models:

- **Authenticating edge:** the edge validates the delivery key from a local
  snapshot or equivalent native credential facility, applies limits, and uses
  a credential-independent cache key. This gives the best shared-cache
  behavior without adding an origin/auth-service call to every hit.
- **Credential-varying cache:** the CDN includes a credential or signed
  capability in the cache key. This is easier for a limited CDN but fragments
  the cache and requires strict log/redaction handling.
- **Intentionally public commodity delivery:** shared assets are treated as
  public or best-effort protected, while expensive or private origin routes are
  authenticated. Origin-only authentication must not be described as
  protecting cache hits that bypass the origin.

An authenticating edge is an optimization and stronger deployment option, not
the portability baseline. With a basic CDN, the honest choices are usually a
bearer URL/capability in the ordinary cache key, intentionally public shared
content, an authenticating component placed before cache access, or bypassing
the CDN for protected traffic. Each has a different cost, leakage, and cache-hit
trade-off.

`Origin` or `Referer` checks can be an optional anti-hotlink signal at the edge,
but they are forgeable and are not authentication. Varying origin caches by
these headers should require evidence that the policy benefit exceeds the cache
fragmentation.

Authentication alone does not imply `Cache-Control: private` or `no-store`.
Those directives are required when the response is personalized, contains a
credential, or has confidentiality requirements. Identical, credential-free
responses can remain shareable when an authenticating edge enforces access
before its cache.

## 7. Human management and automated publishing

### 7.1 Human administration

The management surface should integrate with a corporate identity provider
through standard OIDC rather than a vendor-specific login protocol. A typical
web flow uses the authorization-code flow, normally with PKCE, and terminates in
a server-side session with `Secure`, `HttpOnly`, and appropriate `SameSite`
cookies. Mutating cookie-based requests need CSRF protection.

One deployment may configure several named OIDC providers at the same time—for
example, the company's primary IdP and a partner organization. Each provider
has its own allowlisted issuer, client configuration, claim mapping, and
operational status. The verified identity is keyed by `(issuer, subject)`, not
by an email address. Linking identities across issuers is an explicit audited
management action; matching email strings do not link accounts automatically.

The management verifier should enforce an allowlisted issuer, an explicit
management audience, short session validity, and bounded role/group mapping.
MFA and account lifecycle remain the IdP's responsibility. An existing locally
verifiable session may continue until its expiry during an IdP interruption;
new login or refresh fails closed. Mutations produce an audit record containing
actor, action, target, outcome, and request/trace identity.

Management and publishing routes should use a distinct listener, hostname, or
ingress policy where practical. They must not inherit the delivery CDN's shared
cache rules, and sensitive responses should be `no-store`. OIDC is an identity
mechanism, not a substitute for restricting unnecessary network exposure.

The management API may create, rotate, disable, or scope delivery credentials,
but possession of one of those delivery credentials never grants access back to
the management API.

### 7.2 Automated publishing

CI jobs, importers, and content pipelines should use workload identity or a
narrow service credential. Their permissions should describe publishing
actions and content namespaces, not a human role. Long-lived static credentials
are a portability fallback, not the preferred production mechanism.

Human CLI access may use an OIDC device or browser flow if needed. Automation
must not depend on completing an interactive login.

Human OIDC sessions and workload credentials may coexist on the management
plane, but they represent different principal kinds. Authorization policy must
distinguish a human actor from a publishing workload even when both are allowed
to invoke one management API.

### 7.3 Biei to Ishikari

Biei calls Ishikari as Biei, using a dedicated internal service identity. It
does not forward the user's delivery credential as proof of its own authority.
The original customer/project identity may be propagated separately as trusted,
bounded attribution metadata only when the transport authenticates Biei and the
receiving route explicitly expects it. Biei derives and overwrites that metadata
after verification; it never relays a client-supplied identity header unchanged.

This separation supports distributed tracing and attribution without making
Ishikari trust arbitrary client-supplied identity headers.

## 8. Portable verifier seam and composition

The core request path should depend on a small verifier interface that returns
a typed principal or a bounded failure reason. Useful deployment adapters may
include:

- `None` for explicitly unauthenticated local development;
- `StaticApiKeys` for the first built-in delivery implementation;
- `TrustedHeader` behind an authenticated reverse proxy that strips incoming
  copies of the trusted headers;
- external JWT validation with locally cached keys; and
- an OIDC-backed management-session verifier on management routes.

These adapters are selectable building blocks, not an implicit `try each until
one succeeds` chain. Concurrent mechanisms require an unambiguous dispatch rule
based on the route and credential carrier or scheme:

- presenting credentials for more than one mechanism is rejected rather than
  resolved by precedence;
- once a mechanism recognizes its credential, failure is final and cannot fall
  through to `None`, `TrustedHeader`, or another weaker verifier;
- every successful verifier returns the same small canonical principal shape,
  including its credential plane and authentication method; and
- authorization consumes that principal without depending on provider-specific
  raw claims.

The first delivery-auth deployment should configure exactly one mechanism,
normally `StaticApiKeys`. Supporting multiple delivery mechanisms concurrently
adds downgrade, cache, metrics, and incident-response complexity and should be
introduced only for a concrete migration or federation requirement. In
contrast, multiple named OIDC providers and a distinct workload credential are
reasonable management-plane requirements because those principal populations
are inherently different.

Networked forward-auth can remain an integration escape hatch, but it should
not be the default delivery path because it adds latency, cost, and another
availability dependency to every cache hit.

The `None` mode must be explicit and visible in startup logs and health/config
diagnostics. Production manifests should not silently fall back to it because a
secret or issuer setting is missing.

Cloud-specific workload identity belongs in deployment adapters. Core crates
should consume a service credential or authenticated transport abstraction and
must not require GKE-specific metadata APIs.

## 9. Strong/private access and URL capabilities

Some future products may need stronger properties than the ordinary delivery
tier: confidential tilesets, end-user authorization, near-immediate revocation,
or per-document grants. Those requirements justify a distinct route/tier with
short-lived credentials, explicit authorization, conservative caching, and a
fail-closed control-plane policy. They should not silently make every public
glyph or tile request pay the same cost.

Signed URL capabilities remain an optional answer to a specific client/CDN
constraint, not the default architecture. Before adopting them, a concrete
design must resolve:

- which trusted component signs them and how signing authority is isolated;
- expiry and clock-skew behavior for long-running map clients;
- key rotation and revocation expectations;
- canonical URL/path encoding;
- CDN cache fragmentation and cache-key behavior;
- leakage through URLs, logs, referrers, and copied styles; and
- whether the CDN actually validates the capability or merely varies on it.

A capability URL is still a bearer credential. Embedding one in a style does
not create an end-user identity, and a dumb CDN does not become an authorization
service merely because the URL contains a signature.

## 10. Configuration and registry distribution

The first delivery implementation does not require an online management system
or object-store registry. A bounded static key set supplied through deployment
configuration or a mounted secret is sufficient to validate the request path,
metrics, limits, and operational behavior.

If dynamic credential management becomes necessary, a registry may publish
immutable snapshots to object storage or another distribution channel. The
minimum properties are:

- a single writer or compare-and-swap semantics for management mutations;
- immutable, validated snapshots with an explicit revision;
- atomic replacement of the in-process reader view;
- last-known-good operation and freshness telemetry;
- separate read and write identities; and
- no per-request dependency on registry storage.

This is a control-plane optimization and availability mechanism, not a reason
to invent a general database inside either server. The schema should be driven
by implemented policy rather than speculative claim fields.

Registry freshness and token-revocation latency need explicit metrics and an
alerting threshold before a registry is relied on operationally.

The maximum acceptable snapshot age is a tier-specific availability decision,
not a universal hard-coded timeout. Commodity delivery may intentionally keep
serving a last-known-good snapshot while raising a loud stale-registry alert;
strong/private access should fail closed after its documented bound.

## 11. Code and ownership boundaries

Authentication should preserve the current separation between entry-point
configuration and reusable service code:

- `servers/*` reads environment/configuration and assembles verifier adapters;
- service core crates consume typed verifier/configuration objects;
- domain routers own route-level authorization decisions;
- cache keys describe representations, not credentials;
- gossip and internal wire formats do not carry raw external credentials; and
- simulators model authentication cost only when a measured question requires
  it.

Do not extract a shared authentication crate merely because both servers may
eventually authenticate requests. Keep the first implementation with its owner,
then extract only stable, service-independent primitives with at least two real
consumers.

Errors exposed to callers should remain coarse (`missing`, `invalid`,
`forbidden`, or temporarily unavailable where appropriate). Detailed verifier
or registry failures belong in bounded internal telemetry and must not reveal
secret material.

## 12. Suggested adoption order

1. Define route classes and decide which current routes actually require
   delivery authentication.
2. Add a local `StaticApiKeys` verifier, typed delivery principal, bounded
   metrics, and tests proving authentication happens before expensive work.
3. Add edge request/egress limits and validate CDN log-based usage accounting.
4. Add the dedicated Biei-to-Ishikari service identity before treating internal
   routes as trusted.
5. Add portable OIDC management and workload publishing identity when a
   management/publishing API exists.
6. Introduce a dynamic registry only when static distribution is an observed
   operational constraint.
7. Add signed capabilities or a strong private-data tier only after their
   client, CDN, confidentiality, and revocation requirements are concrete.

Each stage should have a deployable rollback and must preserve shared-cache
behavior unless the security requirement explicitly calls for isolation.

## 13. Acceptance criteria for the first delivery-auth change

Before enabling it in the demo or production-like deployment, tests and metrics
should demonstrate that:

- missing, malformed, unknown, and disabled keys fail closed;
- a valid, authorized key reaches both cached and uncached responses;
- unauthorized requests consume no render or remote-storage admission;
- credentials and unbounded identifiers do not appear in logs or metric labels;
- equivalent authorized callers share the same semantic cache entry;
- registry/config refresh failure keeps only a last-known-good snapshot, never
  an implicit allow-all state;
- startup without required verification material remains unready or rejects the
  protected routes;
- Biei's internal credential, rather than the caller's delivery key, is used on
  Biei-to-Ishikari requests;
- an invalid recognized credential never falls through to another mechanism;
- requests carrying credentials for multiple mechanisms are rejected; and
- the deployment documents every CDN capability assumed by its access,
  enforcement, cache, and accounting claims.

Latency and CPU measurements should include cache-hit-heavy traffic. A verifier
that looks cheap only beside a cold render can still be a significant regression
for glyph, sprite, metadata, and hot tile requests.
