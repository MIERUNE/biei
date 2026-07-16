# Ishikari Specification

Durable design contract for Ishikari: what it is, what it must not become, and
the invariants and module boundaries the implementation must uphold. Active work
items and open decisions live in `../issues/ishikari-todo.md`. Component-level
contracts are in `isoline-and-hillshade-spec.md` and `simulator-spec.md`.

## Positioning

Ishikari's primary purpose is efficient, low-cost delivery of PMTiles archives
stored in object storage. Its core product is PMTiles-backed TileJSON and tile
bytes over ordinary HTTP, with distributed cache locality and backend range-read
batching.

Style JSON, glyphs, sprites, preview pages, and renderer integration are
supporting provider features. Biei is the primary demo consumer, but Ishikari
must remain a standalone provider and must not grow renderer-specific routing or
worker concepts.

## Non-Goals and Guardrails

- Do not move Biei render routing into Ishikari.
- Do not make Ishikari aware of Biei worker slots, render permits, or render
  output caches.
- Do not require Biei or other consumers to understand PMTiles archives directly.
- Do not create a shared cluster crate until repeated cross-project reuse proves it is worth the abstraction cost.
- Keep entry-side tile requests uncoalesced unless production traffic changes
  materially. A 50-VU, 3-node replay measured identical overlap in only 0.23%
  of peer tile fetches; bootstrap and leaf reads already use effective per-key
  single-flight.
- Do not put attacker-controlled `style_id` or `tileset_id` values in metric
  labels.
- Keep `/_internal/*`, including `/_internal/metrics`, on the cluster-internal
  listener (`ISKR_INTERNAL_HTTP_PORT`) only. Never route that port through a
  Service, Gateway, or Ingress, and keep the public listener returning 404 for
  those paths.
- Keep the headless gossip Service gossip-only; do not publish public HTTP
  `8080` there.
- Keep PMTiles parsing in `pmtiles/`, byte access and peer routing in `storage/`,
  and style/glyph/sprite provider logic outside PMTiles archive parsing.

## Refactor Direction

Do not move files only for aesthetics. Move modules when a new responsibility
needs a clearer boundary.

Likely future splits:

- split `server` into `http` when response shaping, request IDs, metrics, and resource families make the current module too broad;
- split provider-resource fetch/cache code if more metrics or resource kinds make `upstream.rs` hard to reason about.
