# biei k8s demo

Runs a 3-node biei rendering cluster. The local overlay renders against remote
style/tile providers. The GKE overlay attaches biei to the shared demo Gateway
and can also render styles/tiles served by the in-cluster provider.

This is a demo, not a production deployment.

## Layout

| Path | Purpose |
|---|---|
| `Dockerfile` | Linux OpenGL/EGL image using Mesa llvmpipe for headless rendering. |
| `k8s/base/` | Deployment, ClusterIP HTTP Service, and headless gossip Service. |
| `k8s/overlays/local/` | Local Kubernetes overlay; exposes `svc/biei` with a local LoadBalancer. |
| `k8s/overlays/gke/` | GKE overlay; uses Artifact Registry image and a Gateway `HTTPRoute`. |

## Local

```sh
docker build -f demo-deploy/Dockerfile -t biei:dev .
kubectl apply -k demo-deploy/k8s/overlays/local
kubectl -n biei-demo rollout status deploy/biei

curl 'http://localhost:8080/carto/voyager-gl-style/static/139.767,35.681,11/512x384.png' -o tokyo.png
```

If your local Kubernetes has no LoadBalancer controller, port-forward instead:

```sh
kubectl -n biei-demo port-forward svc/biei 8080:8080
```

## GKE

Build and push the image, then deploy the overlay. The shared Gateway
(`demo-gw` in namespace `map-demo`) must already exist. The GKE overlay points
all style and tileset resolution at the in-cluster `ishikari` Service, so deploy
Ishikari's GKE overlay first. Public namespaces such as `/carto/*`, `/mierune/*`,
and `/ishikari/*` are style-id prefixes under Ishikari's backing store.

```sh
gcloud builds submit --config demo-deploy/cloudbuild.yaml .

DIGEST="$(gcloud artifacts docker images describe \
  asia-northeast1-docker.pkg.dev/mappf-experiment/biei/biei:dev \
  --format='value(image_summary.digest)')"

# Keep the GKE overlay pinned to the exact image that was just built. This
# avoids ambiguous rollouts when the mutable :dev tag is reused.
export DIGEST
python3 - <<'PY'
import os
from pathlib import Path
path = Path("demo-deploy/k8s/overlays/gke/kustomization.yaml")
text = path.read_text()
lines = []
for line in text.splitlines():
    if line.strip().startswith("digest: sha256:"):
        line = f"    digest: {os.environ['DIGEST']}"
    lines.append(line)
path.write_text("\\n".join(lines) + "\\n")
PY

kubectl apply -k demo-deploy/k8s/overlays/gke
kubectl -n map-demo rollout status deploy/biei
```

The shared Gateway uses the Certificate Manager map
`mappf-demo-cert-map`. Add Biei's hostname to that map once:

```sh
gcloud services enable certificatemanager.googleapis.com

gcloud certificate-manager dns-authorizations create mappf-biei-demo \
  --domain=biei-demo.mierune.dev \
  --location=global

gcloud certificate-manager certificates create mappf-biei-demo-cert \
  --domains=biei-demo.mierune.dev \
  --dns-authorizations=mappf-biei-demo \
  --location=global

gcloud certificate-manager maps entries create biei-demo \
  --map=mappf-demo-cert-map \
  --hostname=biei-demo.mierune.dev \
  --certificates=mappf-biei-demo-cert \
  --location=global
```

Add the DNS authorization CNAME shown by:

```sh
gcloud certificate-manager dns-authorizations describe mappf-biei-demo \
  --location=global \
  --format='value(dnsResourceRecord.name,dnsResourceRecord.type,dnsResourceRecord.data)'
```

Public Gateway routes are intentionally limited to render namespaces:

- `/carto/*`
- `/mierune/*`
- `/ishikari/*`

`/_internal/*` is not routed by the Gateway.
The shared Gateway listens on HTTPS only.

## Checks

```sh
kubectl -n map-demo port-forward deploy/biei 8080:8080

curl 'http://localhost:8080/carto/voyager-gl-style/static/[139.6,35.6,139.9,35.8]/512x384.png?padding=20' -o bbox.png
curl 'http://localhost:8080/carto/voyager-gl-style/8/227/100.png' -o tile.png

# In the GKE overlay, all styles are fetched through Ishikari. The requested
# style id must exist under Ishikari's STYLE_TEMPLATES backing store, for
# example styles/carto/voyager-gl-style/style.json for the first URL above.
curl 'http://localhost:8080/ishikari/<style_id>/static/139.767,35.681,11/512x384.png' -o ishikari.png

curl -s localhost:8080/_internal/readyz
curl -s localhost:8080/_internal/metrics
```

## Notes

- The demo uses a Deployment, not a StatefulSet; chitchat handles dynamic
  membership.
- The headless gossip Service uses `publishNotReadyAddresses: true` so pods can
  discover each other during cold start.
- Software rendering is CPU-bound. Tune `BIEI_CORES` and CPU limits together.
