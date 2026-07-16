# Biei

A distributed renderer for static map images and tiles, built with MapLibre Native.

> [!WARNING]
> This is an experimental, proof-of-concept project. The behavior, API, and configuration are not stable.

Biei is designed to work both as a simple single-node server and as a scalable MapLibre rendering cluster:

- **Static image rendering** - renders center, bbox, auto-fit, path, GeoJSON, pin, and `addlayer` requests.
- **Rasterized tile rendering** - serves pre-rendered raster tiles from MapLibre styles.
- **Scale-out render pool** - adds gossip membership, peer forwarding, and rendered-image caching when run as a multi-node cluster.

LICENSE: MIT OR Apache-2.0

## Demo

To start a simple single-node server:

```sh
cargo run -p biei -- \
  --style-templates 'carto=https://basemaps.cartocdn.com/{style_id}/style.json'
open http://localhost:8080/carto/gl/voyager-gl-style/static/139.767,35.681,11,0,0/640x360@2x.webp
```

To start a local three-node cluster for development:

```sh
bash scripts/dev-cluster.sh
open http://localhost:8080/carto/gl/voyager-gl-style/preview
```

The script builds `biei`, starts `NUM_NODES` processes on consecutive
HTTP/gossip ports, prefixes logs by node, and stops all nodes on Ctrl-C.

Sample URLs against the default local cluster (`BASE_HTTP_PORT=8080`):

```text
# tile rendering preview page
http://localhost:8080/carto/gl/voyager-gl-style/preview

# static center image around Tokyo
http://localhost:8080/carto/gl/voyager-gl-style/static/139.767,35.681,11,0,0/640x360@2x.webp

# static bbox image
http://localhost:8080/carto/gl/voyager-gl-style/static/[139.55,35.55,139.95,35.85]/640x360@2x.webp

# route-style overlay: blue pin, path, red pin
http://localhost:8080/carto/gl/voyager-gl-style/static/path-5+1a75ff-0.8(g%7DwxEwfatY_q%40vaLgbC_vJ),pin-l-s+1a75ff(139.767,35.681),pin-l-g+fd3344(139.760,35.710)/auto/640x360@2x.webp

# GeoJSON polygon overlay with auto fit
http://localhost:8080/carto/gl/voyager-gl-style/static/geojson(%7B%22type%22%3A%22Feature%22%2C%22properties%22%3A%7B%22fill%22%3A%22%2345cf23%22%2C%22fill-opacity%22%3A0.35%2C%22stroke%22%3A%22%23333%22%2C%22stroke-width%22%3A2%7D%2C%22geometry%22%3A%7B%22type%22%3A%22Polygon%22%2C%22coordinates%22%3A%5B%5B%5B139.65%2C35.62%5D%2C%5B139.85%2C35.62%5D%2C%5B139.85%2C35.78%5D%2C%5B139.65%2C35.78%5D%2C%5B139.65%2C35.62%5D%5D%5D%7D%7D)/auto/640x360@2x.webp

# raster tile
http://localhost:8080/carto/gl/dark-matter-gl-style/5/28/12@2x.webp
```

Override ports or providers with environment variables. If you change
`BASE_HTTP_PORT`, replace `8080` in the sample URLs with that port.

```sh
NUM_NODES=4 BASE_HTTP_PORT=18080 BASE_INTERNAL_PORT=19090 BASE_GOSSIP_PORT=17946 \
STYLE_URL_TEMPLATE='carto=https://basemaps.cartocdn.com/{style_id}/style.json' \
bash scripts/dev-cluster.sh
```

Single-node mode is the default. Cluster mode is explicit and serves two HTTP
listeners: a public port (`--http-bind`, default `:8080`) for render ingress plus
top-level `/livez` `/readyz`, and a separate cluster-internal port
(`--internal-port`, default `9090`) for `/_internal/*` (including metrics) and
peer-to-peer forwarding. The internal port is never exposed publicly; peers
forward to the advertised internal address, so `--internal-advertise-addr` points at the
internal port:

```sh
cargo run -p biei -- \
  --cluster \
  --style-templates 'http://style-provider.svc.cluster.local:8080/styles/{style_id}/style.json' \
  --tileset-url-template 'http://style-provider.svc.cluster.local:8080/tilesets/{tileset_id}/tileset.json' \
  --mln-resource-private-hosts style-provider.svc.cluster.local \
  --mln-resource-cache-bytes 268435456 \
  --internal-port 9090 \
  --internal-advertise-addr "$HOSTNAME.biei.default.svc.cluster.local:9090" \
  --gossip-seeds biei-0.biei:7946
```

### Style templates

`--style-templates` (env `BIEI_STYLE_TEMPLATES`) maps a request's style id to a
`style.json` URL. It is a `;`-separated list of entries; each `<template>` must
be an http(s) URL with `{style_id}` in its path. Placeholders in the authority,
query, or fragment are rejected.

**Single bare template** — every style id is substituted whole:

```sh
--style-templates 'https://basemaps.cartocdn.com/{style_id}/style.json'
# request path          style id            -> resolved style.json
# /gl/voyager-gl-style  gl/voyager-gl-style -> https://basemaps.cartocdn.com/gl/voyager-gl-style/style.json
# /positron             positron            -> https://basemaps.cartocdn.com/positron/style.json
```

**Multiple `namespace=<template>` entries** (+ optional `default=`) — the style
id's **first path segment** picks the template. On a namespace match that
segment is stripped, so only the rest fills `{style_id}`; the `default` (or a
bare entry) is the catch-all and receives the whole id:

```sh
--style-templates '
  carto=https://basemaps.cartocdn.com/{style_id}/style.json;
  example=https://styles.example.test/{style_id}/style.json;
  default=https://basemaps.cartocdn.com/{style_id}/style.json'
```

Without a `default`, an unregistered namespace returns `unknown_style` (404),
which keeps the catalog scoped to providers you list.

### Cache knobs

`BIEI_SOURCE_CACHE_CAPACITY` controls the per-renderer warm source cache
capacity (default `1`). `BIEI_RENDER_OUTPUT_CACHE_BYTES` controls the node-local
rendered image cache size (default `268435456`, set `0` to disable).
Rendered entries expire after five minutes even when the style revision is
unchanged, because referenced resources may change at stable URLs.
`BIEI_MLN_RESOURCE_CACHE_BYTES` controls the process-wide in-memory cache for
tiles, glyphs, sprites, and other MapLibre resources (default `268435456`, set
`0` to disable). The resource cache is shared by every renderer slot in the
process and does not persist across restarts.
`BIEI_MLN_BODY_PERMITS` bounds concurrent response-body buffering and defaults
to `max(24, 4 * render_permits)`; tune it only when admission-wait and memory
metrics show that the default is inappropriate.

The `biei_mln_resource_*` metrics separate Database cache operations, deferred
refreshes, admission wait, single-flight participation, and actual upstream
HTTP attempts. Use `--disable-mln-file-sources` only as a diagnostic A/B mode
when comparing the Rust cache/loader with MapLibre Native's default leaves.

Map resources are allowed to resolve to public IP addresses by default. Set
`BIEI_MLN_RESOURCE_PRIVATE_HOSTS` to a comma-separated list of exact hosts or
leading-wildcard domains when an operator-managed style intentionally loads
resources from a private network, for example
`resource-api.default.svc.cluster.local,*.tiles.svc.cluster.local`. Loopback,
link-local, and private addresses reached through any other hostname or redirect
are rejected. Keep this exception as narrow as possible: an allowlisted host
bypasses private-address filtering, so broad service-domain wildcards can expose
unrelated internal services when resource URLs are not fully trusted.

## Simulator

Run one deterministic simulation and write both machine-readable and
self-contained visual reports:

```sh
cargo run -p biei-sim -- run \
  --report biei-sim-report.json \
  --html biei-sim-report.html
```

Membership churn is replayed from an ordered JSON plan and sampled before and
after every event. `at_request` counts measured requests after the configured
warmup, so high-rate runs cannot hide churn inside the warmup window:

```json
{"events":[
  {"at_request":500,"action":"add"},
  {"at_request":1500,"action":"remove","node_id":"node-0"}
]}
```

```sh
cargo run -p biei-sim -- run \
  --churn-plan biei-sim/examples/churn-plan.json \
  --report churn-report.json
cargo run -p biei-sim -- visualize churn-report.json --output churn-report.html
```

Events beyond the generated measured workload do not discard the run. They are
preserved under `unapplied_events` in the JSON/HTML report and produce a CLI
warning. A near-saturation churn run can be exercised with:

```sh
cargo run -p biei-sim -- run --nodes 4 --styles 20 --rate 3000 \
  --duration-seconds 10 --warmup-seconds 2 \
  --churn-plan biei-sim/examples/churn-plan.json \
  --report churn-report.json --html churn-report.html
```

The churn report includes per-sample counter deltas and interval p50/p99/max
latency, rather than requiring cumulative counters to be interpreted by eye.

Running `cargo run -p biei-sim` without a subcommand retains the legacy sweep
suite.

Export a time-bounded, immutable production calibration snapshot from a
Prometheus API before changing simulator costs or production permit defaults:

```sh
cargo run -p biei-sim -- calibration exercise \
  --url 'http://localhost:8080/carto/gl/voyager-gl-style/0/0/0@2x.webp' \
  --url 'http://localhost:8080/carto/gl/voyager-gl-style/static/139.767,35.681,11,0,0/640x360@2x.webp' \
  --warmup-requests-per-url 2 --requests-per-url 100 --concurrency 4
```

The exercise prints the measured Unix-time window to pass to the exporter. It
is deliberately bounded: calibration needs representative stage samples, not
production-scale traffic. Its default 30-second settle period before and after
measurement assumes a Prometheus scrape interval of at most 30 seconds; adjust
`--scrape-settle-seconds` to match the deployment.

```sh
END=$(date +%s)
START=$((END - 900))
cargo run -p biei-sim -- calibration export \
  --prometheus-url "$PROMETHEUS_URL" \
  --start-unix-seconds "$START" --end-unix-seconds "$END" \
  --match-label namespace=map-demo --match-label container=biei \
  --deployment-revision "$DEPLOYMENT_REVISION" \
  --architecture x86_64 --hardware-profile "$HARDWARE_PROFILE" \
  --cpu-cores-per-node 2 --renderer-slots-per-node 3 \
  --execution-permits-per-node 2 --native-render-permits-per-node 2 \
  --output "calibration-${DEPLOYMENT_REVISION}-${START}-${END}.json"
```

`PROMETHEUS_URL` is the Prometheus server/API root, not biei's raw metrics
endpoint. Google Managed Service for Prometheus roots are supported; pass an
OAuth access token through `--bearer-token-file`, never through `--notes` or the
URL. Existing output files are not overwritten. Importing this schema into a
simulation is available through `biei-sim run --cost-profile <snapshot>`. The
importer keeps workload-weighted global ranges for routing decisions, while
renderer sleeps sample the recorded distributions by mode, scale, format, size,
and warm/cold/swap state. When an exact shape is sparse, sampling falls back to
the corresponding state aggregate and then to the simulator default. It applies
the recorded core/slot/permit layout and writes structured coverage
(`measured`, `derived`, or `default`), sample counts, and all approximation or
fallback notes into the run report. The report also records how many exact
shape and aggregate fallback samplers were built. Profiles may be partial:
usable setup or render stages are applied independently. CPU/resource splitting remains an
approximation, and the result is not sizing evidence until validated against
production end-to-end distributions.
