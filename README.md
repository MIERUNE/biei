# Biei + Ishikari

This repository contains the two services that make up the map rendering
stack:

- [Biei](services/biei/README.md) is the distributed MapLibre Native renderer.
- [Ishikari](services/ishikari/README.md) serves and caches PMTiles, styles,
  glyphs, sprites, and derived terrain products.

Biei resolves its production map resources through Ishikari, while both
services keep independent cluster membership, scaling, release, and failure
policies.

## Repository layout

```text
services/
  biei/       # Biei Cargo workspace, simulator, and deployment
  ishikari/   # Ishikari Cargo workspace, simulator, terrain crate, and deployment
crates/       # Reserved for small, proven cross-service primitives
integration/  # Cross-service contract and smoke tests
```

The service directories are deliberately separate Cargo workspaces with their
own `Cargo.lock`. Biei enables HTTP decompression and a native compression
backend that Ishikari intentionally does not use; combining the packages into
one Cargo workspace would make workspace-wide feature unification a correctness
risk for Ishikari's representation-preserving proxy path.

## Development

Run commands from the service workspace being changed:

```sh
cd services/biei
cargo test --workspace

cd ../ishikari
cargo test --workspace --all-targets
```

The root GitHub workflows are path-scoped. A service-only change runs that
service's CI; changes under `crates/` run both. Cross-service behavior belongs
in `integration/` rather than in either service's unit-test suite.

## Build and deployment

Each service retains its own image, BuildKit cache, and deployment lifecycle:

```sh
cd services/ishikari
gcloud builds submit --config demo-deploy/cloudbuild.yaml .

cd ../biei
gcloud builds submit --config demo-deploy/cloudbuild.yaml .
```

The root `kustomization.yaml` composes the shared Gateway and both service GKE
overlays. Existing service-local deployment paths remain authoritative for
independent rollouts; see [deploy/README.md](deploy/README.md) for the combined
stack workflow.

## Shared-code policy

Monorepo placement does not by itself make similarly named code equivalent.
Only behavior-neutral primitives with shared tests should move into `crates/`.
Membership state schemas, routing keys, metrics, internal wire protocols, and
cache/fetch policy remain service-owned unless their contracts are explicitly
aligned first.

LICENSE: MIT OR Apache-2.0
