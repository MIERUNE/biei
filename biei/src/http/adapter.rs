//! Axum adapter for production HTTP ingress.
//!
//! URL parsing and response classification stay in `http::ingress`; this module
//! only binds a socket and converts that small internal response shape into an
//! HTTP response.

use std::net::SocketAddr;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::Response;
use prometheus::{IntGaugeVec, Registry};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::gossip::GossipBus;
use crate::http::REQUEST_ID_HEADER;
use crate::http::ingress::HttpIngress;
use crate::http::response::IngressResponse;
use crate::metrics::encode_metric_families;
use crate::types::RequestId;

const MAX_INTERNAL_FORWARD_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_PUBLIC_PATH_BYTES: usize = 8192;

#[derive(Clone)]
pub struct ShutdownSignal {
    rx: watch::Receiver<bool>,
}

pub fn shutdown_channel() -> (watch::Sender<bool>, ShutdownSignal) {
    let (tx, rx) = watch::channel(false);
    (tx, ShutdownSignal { rx })
}

impl ShutdownSignal {
    async fn wait(mut self) {
        if *self.rx.borrow() {
            return;
        }
        let _ = self.rx.changed().await;
    }
}

#[derive(Clone)]
struct HttpServerState {
    ingress: Option<HttpIngress>,
    ready: bool,
    drain: Option<crate::drain::DrainController>,
    membership: Option<crate::membership::Membership>,
    internal_forward: Option<crate::http::internal::InternalForwardEndpoint>,
    metrics: Option<HttpMetrics>,
}

#[derive(Clone)]
pub struct HttpMetrics {
    node: crate::node::Node,
    membership: Option<crate::membership::Membership>,
    drain: Option<crate::drain::DrainController>,
}

impl HttpMetrics {
    pub fn new(
        node: crate::node::Node,
        membership: Option<crate::membership::Membership>,
        drain: Option<crate::drain::DrainController>,
    ) -> Self {
        Self {
            node,
            membership,
            drain,
        }
    }

    async fn render_prometheus(&self) -> String {
        let node_id = self.node.id();
        let node = node_id.as_str();
        let workers = self.node.worker_snapshot();
        let registry = Registry::new();
        let queue_depth = IntGaugeVec::new(
            prometheus::Opts::new(
                "biei_queue_depth",
                "Current queued tasks per renderer worker.",
            ),
            &["node", "worker", "style_id", "render_mode", "scale"],
        )
        .expect("valid queue gauge");
        let worker_loaded = IntGaugeVec::new(
            prometheus::Opts::new(
                "biei_worker_loaded",
                "Whether a renderer worker has a loaded profile.",
            ),
            &["node", "worker"],
        )
        .expect("valid worker-loaded gauge");
        let membership_size = IntGaugeVec::new(
            prometheus::Opts::new("biei_membership_size", "Current membership size by state."),
            &["node", "state"],
        )
        .expect("valid membership gauge");
        let cpu_permits_inuse = IntGaugeVec::new(
            prometheus::Opts::new(
                "biei_cpu_permits_inuse",
                "Currently held CPU/GPU render-stage permits.",
            ),
            &["node"],
        )
        .expect("valid cpu permits gauge");
        let drain_state = IntGaugeVec::new(
            prometheus::Opts::new("biei_drain_state", "Whether the node is draining."),
            &["node"],
        )
        .expect("valid drain-state gauge");

        for collector in [
            Box::new(queue_depth.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(worker_loaded.clone()),
            Box::new(membership_size.clone()),
            Box::new(cpu_permits_inuse.clone()),
            Box::new(drain_state.clone()),
        ] {
            registry
                .register(collector)
                .expect("register dynamic biei metric");
        }

        for worker in &workers {
            let profile = worker.loaded_profile.as_ref();
            let style_id = profile.map(|p| p.style.id.as_str()).unwrap_or_default();
            let render_mode = profile
                .map(|p| p.render_mode.as_gossip_value())
                .unwrap_or("none");
            let scale = profile.map(|p| p.scale.as_gossip_value()).unwrap_or("none");
            queue_depth
                .with_label_values(&[node, &worker.id.to_string(), style_id, render_mode, scale])
                .set(worker.queue_depth as i64);
            let loaded = usize::from(worker.loaded_profile.is_some());
            worker_loaded
                .with_label_values(&[node, &worker.id.to_string()])
                .set(loaded as i64);
        }
        if let Some(membership) = &self.membership {
            let live = membership.view().await.members.len();
            membership_size
                .with_label_values(&[node, "live"])
                .set(live as i64);
        }
        cpu_permits_inuse
            .with_label_values(&[node])
            .set(self.node.cpu_permits_inuse() as i64);
        let draining = self.drain.as_ref().is_some_and(|drain| drain.is_draining());
        drain_state
            .with_label_values(&[node])
            .set(i64::from(draining));

        let mut families = self.node.metrics().gather();
        families.extend(registry.gather());
        encode_metric_families(&families)
    }
}

pub async fn serve_with_shutdown(
    ingress: HttpIngress,
    bind: SocketAddr,
    shutdown: Option<ShutdownSignal>,
) -> anyhow::Result<()> {
    serve_with_shutdown_and_membership(ingress, bind, shutdown, None).await
}

pub async fn serve_with_shutdown_and_membership(
    ingress: HttpIngress,
    bind: SocketAddr,
    shutdown: Option<ShutdownSignal>,
    membership: Option<crate::membership::Membership>,
) -> anyhow::Result<()> {
    serve_with_shutdown_and_membership_and_internal_forward(
        ingress, bind, shutdown, membership, None,
    )
    .await
}

pub async fn serve_with_shutdown_and_membership_and_internal_forward(
    ingress: HttpIngress,
    bind: SocketAddr,
    shutdown: Option<ShutdownSignal>,
    membership: Option<crate::membership::Membership>,
    internal_forward: Option<crate::http::internal::InternalForwardEndpoint>,
) -> anyhow::Result<()> {
    let drain = ingress.drain_controller();
    let metrics = Some(HttpMetrics::new(
        ingress.node(),
        membership.clone(),
        drain.clone(),
    ));
    serve_with_state(
        HttpServerState {
            drain,
            ingress: Some(ingress),
            ready: true,
            membership,
            internal_forward,
            metrics,
        },
        bind,
        shutdown,
    )
    .await
}

async fn serve_with_state(
    state: HttpServerState,
    bind: SocketAddr,
    shutdown: Option<ShutdownSignal>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind HTTP listener on {bind}"))?;
    let server = axum::serve(listener, router(state));
    if let Some(signal) = shutdown {
        server
            .with_graceful_shutdown(signal.wait())
            .await
            .context("serve HTTP listener")?;
    } else {
        server.await.context("serve HTTP listener")?;
    }
    Ok(())
}

fn router(state: HttpServerState) -> Router {
    Router::new().fallback(handle).with_state(state)
}

async fn handle(
    State(state): State<HttpServerState>,
    method: Method,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    if uri.path() == "/_internal/healthz" {
        if method != Method::GET {
            return simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return simple_response(StatusCode::OK, "ok");
    }
    if uri.path() == "/_internal/readyz" {
        if method != Method::GET {
            return simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let ready = state.ready
            && state
                .drain
                .as_ref()
                .is_none_or(|drain| !drain.is_draining());
        let ready = ready
            && match &state.membership {
                Some(membership) => membership.is_gossip_ready().await,
                None => true,
            };
        return if ready {
            simple_response(StatusCode::OK, "ready")
        } else {
            simple_response(StatusCode::SERVICE_UNAVAILABLE, "not ready")
        };
    }
    if uri.path() == "/_internal/metrics" {
        if method != Method::GET {
            return simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let Some(metrics) = state.metrics else {
            return simple_response(StatusCode::NOT_FOUND, "metrics disabled");
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")
            .body(Body::from(metrics.render_prometheus().await))
            .unwrap_or_else(|_| {
                simple_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "metrics response build failed",
                )
            });
    }

    if uri.path() == "/_internal/forward" {
        if method != Method::POST {
            return simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let Some(internal_forward) = state.internal_forward else {
            return simple_response(StatusCode::NOT_FOUND, "internal forward disabled");
        };
        let headers = request.headers().clone();
        let body = match to_bytes(request.into_body(), MAX_INTERNAL_FORWARD_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) => return simple_response(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
        };
        return internal_forward.handle(&headers, body).await;
    }

    if method != Method::GET {
        return simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if uri.path().len() > MAX_PUBLIC_PATH_BYTES {
        return simple_response(StatusCode::URI_TOO_LONG, "path too long");
    }
    let Some(ingress) = state.ingress else {
        return simple_response(StatusCode::NOT_FOUND, "not found");
    };
    let request_id = request_id_from_headers(request.headers());
    if is_preview_path(uri.path()) {
        return into_axum_response(ingress.serve_preview(uri.path(), request_id).await);
    }
    into_axum_response(
        ingress
            .handle_path_with_request_id(uri.path(), uri.query(), request_id, Instant::now())
            .await,
    )
}

/// `/{user}/{style}/preview` または `/{style_id}/preview` だけを対象にする。
/// 一般の tile / static 描画 path とは「最終 segment が literal `preview`」
/// で衝突しない構造になっているのを利用する。
fn is_preview_path(path: &str) -> bool {
    let segments: Vec<_> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    matches!(segments.len(), 2 | 3) && segments.last().copied() == Some("preview")
}

fn request_id_from_headers(headers: &HeaderMap) -> Option<RequestId> {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(RequestId::from_string)
}

fn into_axum_response(response: IngressResponse) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, response.content_type);
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from(response.body)).unwrap_or_else(|_| {
        simple_response(StatusCode::INTERNAL_SERVER_ERROR, "response build failed")
    })
}

fn simple_response(status: StatusCode, body: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::from(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn axum_response_preserves_status_content_type_and_headers() {
        let response = into_axum_response(IngressResponse {
            status: 503,
            content_type: "application/json",
            headers: vec![("Retry-After", "1".to_string())],
            body: br#"{"error":"queue_full","detail":""}"#.to_vec().into(),
        });

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(response.headers().get("Retry-After").unwrap(), "1");
    }

    #[test]
    fn method_not_allowed_response_is_plain_text() {
        let response = simple_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn is_preview_path_matches_preview_suffix_only() {
        // Two-segment style + preview
        assert!(is_preview_path("/carto/voyager-gl-style/preview"));
        assert!(is_preview_path("/foo/bar/preview/"));
        // Single-segment style + preview
        assert!(is_preview_path("/voyager-gl-style/preview"));
        // Tile path — last segment is not "preview"
        assert!(!is_preview_path("/carto/voyager/0/0/0@2x.png"));
        // preview not last segment
        assert!(!is_preview_path("/foo/preview/bar"));
        // Too few segments(style id 部分なし)
        assert!(!is_preview_path("/preview"));
        // Too many segments(style id が 2 を超える)
        assert!(!is_preview_path("/foo/bar/baz/preview"));
        // Empty / root
        assert!(!is_preview_path("/"));
        assert!(!is_preview_path(""));
    }

    #[tokio::test]
    async fn health_and_ready_endpoints_are_plain_text() {
        let state = HttpServerState {
            ingress: None,
            ready: true,
            drain: None,
            membership: None,
            internal_forward: None,
            metrics: None,
        };
        let request = Request::builder().body(Body::empty()).unwrap();

        let health = handle(
            State(state.clone()),
            Method::GET,
            "/_internal/healthz".parse().unwrap(),
            request,
        )
        .await;
        assert_eq!(health.status(), StatusCode::OK);

        let request = Request::builder().body(Body::empty()).unwrap();
        let ready = handle(
            State(state),
            Method::GET,
            "/_internal/readyz".parse().unwrap(),
            request,
        )
        .await;
        assert_eq!(ready.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_endpoint_reports_not_ready_while_draining() {
        let drain = crate::drain::DrainController::new();
        let state = HttpServerState {
            ingress: None,
            ready: true,
            drain: Some(drain.clone()),
            membership: None,
            internal_forward: None,
            metrics: None,
        };

        let request = Request::builder().body(Body::empty()).unwrap();
        let ready = handle(
            State(state.clone()),
            Method::GET,
            "/_internal/readyz".parse().unwrap(),
            request,
        )
        .await;
        assert_eq!(ready.status(), StatusCode::OK);

        drain.begin_draining();

        let request = Request::builder().body(Body::empty()).unwrap();
        let ready = handle(
            State(state),
            Method::GET,
            "/_internal/readyz".parse().unwrap(),
            request,
        )
        .await;
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn single_router_routes_public_and_internal_paths() {
        let options = crate::options::Options::try_parse_from([
            "biei",
            "--style-templates",
            "http://style-api.test/styles/{style_id}/style.json",
            "--cores",
            "1",
        ])
        .expect("options parse");
        let runtime = crate::runtime::Runtime::spawn_single_node(&options).expect("runtime");
        let ingress = runtime.http_ingress(Duration::from_secs(2));
        let metrics = Some(HttpMetrics::new(
            runtime.node(),
            None,
            ingress.drain_controller(),
        ));
        let state = HttpServerState {
            drain: ingress.drain_controller(),
            ingress: Some(ingress),
            ready: true,
            membership: None,
            internal_forward: Some(crate::http::internal::InternalForwardEndpoint::with_drain(
                runtime.node(),
                runtime.drain_controller(),
            )),
            metrics,
        };

        let public = handle(
            State(state.clone()),
            Method::GET,
            "/carto/voyager/static/not-an-overlay/auto/256x256.png"
                .parse()
                .unwrap(),
            Request::builder().body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(public.status(), StatusCode::BAD_REQUEST);

        let internal = handle(
            State(state),
            Method::POST,
            "/_internal/forward".parse().unwrap(),
            Request::builder()
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await;
        assert_eq!(internal.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn metrics_endpoint_reports_worker_queue_depths() {
        let options = crate::options::Options::try_parse_from([
            "biei",
            "--style-templates",
            "http://style-api.test/styles/{style_id}/style.json",
            "--cores",
            "1",
        ])
        .expect("options parse");
        let runtime = crate::runtime::Runtime::spawn_single_node(&options).expect("runtime");
        let state = HttpServerState {
            ingress: None,
            ready: true,
            drain: None,
            membership: None,
            internal_forward: None,
            metrics: Some(HttpMetrics::new(runtime.node(), None, None)),
        };

        let response = handle(
            State(state),
            Method::GET,
            "/_internal/metrics".parse().unwrap(),
            Request::builder().body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("metrics body");
        let body = std::str::from_utf8(&body).expect("utf8 metrics");
        assert!(body.contains("# TYPE biei_queue_depth gauge"));
        assert!(body.contains("biei_worker_loaded"));
        assert!(body.contains("biei_cpu_permits_inuse"));
        assert!(body.contains("biei_drain_state"));
        assert!(body.contains("# TYPE biei_tasks_completed_total counter"));
        assert!(body.contains(r#"scope="ingress"} 0"#));
    }

    #[tokio::test]
    async fn public_ingress_echoes_supplied_request_id() {
        let options = crate::options::Options::try_parse_from([
            "biei",
            "--style-templates",
            "http://style-api.test/styles/{style_id}/style.json",
            "--cores",
            "1",
        ])
        .expect("options parse");
        let runtime = crate::runtime::Runtime::spawn_single_node(&options).expect("runtime");
        let ingress = runtime.http_ingress(Duration::from_secs(2));
        let state = HttpServerState {
            ingress: Some(ingress),
            ready: true,
            drain: None,
            membership: None,
            internal_forward: None,
            metrics: None,
        };

        let response = handle(
            State(state),
            Method::GET,
            "/bad".parse().unwrap(),
            Request::builder()
                .header(REQUEST_ID_HEADER, "req-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(REQUEST_ID_HEADER).unwrap(),
            "req-123"
        );
    }

    #[tokio::test]
    async fn public_path_limit_rejects_oversized_paths_before_ingress() {
        let state = HttpServerState {
            ingress: None,
            ready: true,
            drain: None,
            membership: None,
            internal_forward: None,
            metrics: None,
        };
        let long_path = format!("/{}", "x".repeat(MAX_PUBLIC_PATH_BYTES + 1));

        let response = handle(
            State(state),
            Method::GET,
            long_path.parse().unwrap(),
            Request::builder().body(Body::empty()).unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
    }
}
