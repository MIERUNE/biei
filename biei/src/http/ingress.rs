//! URL parsing for the static image / tile API ingress.
//!
//! This module deliberately stops before axum. It converts an already matched
//! request path into an `InternalTask`, so the grammar and validation are
//! testable without binding sockets.
//!
//! This is not a resource loader. Fetching style.json dependencies such as
//! tiles, glyphs, and sprites remains delegated to maplibre-native's default
//! resource loader in production v0.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::drain::DrainController;
use crate::http::error::IngressError;
use crate::http::preview::{PREVIEW_STYLE_CHECK_TIMEOUT, build_preview_response};
use crate::http::response::{IngressResponse, response_from_ingress_error, response_from_outcome};
use crate::node::Node;
use crate::style_catalog::StyleCatalog;
use crate::tileset_catalog::TilesetCatalog;
use crate::types::{
    AddLayer, AddLayerSource, ImageFormat, InternalTask, Padding, PixelRatio, Positioning,
    RenderRequest, RequestId, Scale, StyleId, StyleRevision, TaskId,
};

const MAX_STATIC_WIDTH: u16 = 1920;
const MAX_STATIC_HEIGHT: u16 = 1280;
const MAX_STATIC_RGBA_BYTES: u64 = MAX_STATIC_WIDTH as u64 * MAX_STATIC_HEIGHT as u64 * 2 * 2 * 4;
const TILE_SIZE: u16 = 512;

#[derive(Clone)]
pub struct HttpIngress {
    node: Node,
    catalog: Arc<StyleCatalog>,
    tileset_catalog: Arc<TilesetCatalog>,
    sla_budget: Duration,
    next_task_id: Arc<AtomicU64>,
    drain: Option<DrainController>,
    concurrency: Option<Arc<Semaphore>>,
}

impl HttpIngress {
    pub fn with_drain_and_limit(
        node: Node,
        catalog: Arc<StyleCatalog>,
        tileset_catalog: Arc<TilesetCatalog>,
        sla_budget: Duration,
        drain: DrainController,
        concurrency_limit: usize,
    ) -> Self {
        Self {
            node,
            catalog,
            tileset_catalog,
            sla_budget,
            next_task_id: Arc::new(AtomicU64::new(1)),
            drain: Some(drain),
            concurrency: Some(Arc::new(Semaphore::new(concurrency_limit.max(1)))),
        }
    }

    pub fn drain_controller(&self) -> Option<DrainController> {
        self.drain.clone()
    }

    pub fn node(&self) -> Node {
        self.node.clone()
    }

    #[cfg(test)]
    pub async fn handle_path(&self, path: &str, now: Instant) -> IngressResponse {
        self.handle_path_with_request_id(path, None, None, now)
            .await
    }

    /// Serve the tile-preview HTML page for `/{user}/{style}/preview`(or
    /// single-segment `/{style}/preview`).
    ///
    /// Returns an HTML page that embeds maplibre-gl-js (from CDN) and points it
    /// at biei's own tile endpoint as a raster source. No style.json is needed
    /// because biei serves pre-rendered raster tiles. Unknown style → 404.
    pub async fn serve_preview(
        &self,
        path: &str,
        request_id: Option<RequestId>,
    ) -> IngressResponse {
        let request_id = request_id.unwrap_or_default();
        let _concurrency_permit = match &self.concurrency {
            Some(limit) => match limit.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return IngressResponse::json(503, "ingress_busy", "")
                        .with_retry_after("1")
                        .with_request_id(&request_id);
                }
            },
            None => None,
        };
        let _drain_permit = match &self.drain {
            Some(drain) => match drain.try_acquire() {
                Some(permit) => Some(permit),
                None => {
                    return IngressResponse::json(503, "service_draining", "")
                        .with_retry_after("2")
                        .with_request_id(&request_id);
                }
            },
            None => None,
        };
        let node = self.node.clone();
        build_preview_response(&self.catalog, path, |revision| {
            let node = node.clone();
            async move {
                node.ensure_style_available(&revision, Instant::now() + PREVIEW_STYLE_CHECK_TIMEOUT)
                    .await
            }
        })
        .await
        .with_request_id(&request_id)
    }

    pub async fn handle_path_with_request_id(
        &self,
        path: &str,
        query: Option<&str>,
        request_id: Option<RequestId>,
        now: Instant,
    ) -> IngressResponse {
        let request_id = request_id.unwrap_or_default();
        let _concurrency_permit = match &self.concurrency {
            Some(limit) => match limit.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return IngressResponse::json(503, "ingress_busy", "")
                        .with_retry_after("1")
                        .with_request_id(&request_id);
                }
            },
            None => None,
        };
        let _drain_permit = match &self.drain {
            Some(drain) => match drain.try_acquire() {
                Some(permit) => Some(permit),
                None => {
                    return IngressResponse::json(503, "service_draining", "")
                        .with_retry_after("2")
                        .with_request_id(&request_id);
                }
            },
            None => None,
        };
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let task = match parse_path_with_request_id(
            path,
            query,
            &self.catalog,
            &self.tileset_catalog,
            task_id,
            request_id.clone(),
            self.sla_budget,
            now,
        ) {
            Ok(task) => task,
            Err(err) => return response_from_ingress_error(err).with_request_id(&request_id),
        };
        response_from_outcome(self.node.handle_incoming(task).await)
    }
}

#[cfg(test)]
fn test_tileset_catalog() -> TilesetCatalog {
    TilesetCatalog::new("https://tiles.example.test/{tileset_id}/tileset.json")
}

#[cfg(test)]
fn parse_path(
    path: &str,
    catalog: &StyleCatalog,
    task_id: TaskId,
    sla_budget: Duration,
    now: Instant,
) -> Result<InternalTask, IngressError> {
    parse_path_with_request_id(
        path,
        None,
        catalog,
        &test_tileset_catalog(),
        task_id,
        RequestId::default(),
        sla_budget,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_path_with_request_id(
    path: &str,
    query: Option<&str>,
    catalog: &StyleCatalog,
    tileset_catalog: &TilesetCatalog,
    task_id: TaskId,
    request_id: RequestId,
    sla_budget: Duration,
    now: Instant,
) -> Result<InternalTask, IngressError> {
    let parts: Vec<_> = path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let before_layer = parse_before_layer_from_query(query)?;
    let padding = parse_padding_from_query(query)?;
    let addlayer = parse_addlayer_from_query(query, tileset_catalog)?;
    // tile / static のどちらかを segment 構造で判定する。`static` literal を
    // 中段に含むものは static request、それ以外は tile。preview は別経路で
    // adapter が拾うのでここには来ない。
    if parts.contains(&"static") {
        parse_static_path(
            &parts,
            before_layer,
            padding,
            addlayer,
            catalog,
            task_id,
            request_id,
            sla_budget,
            now,
        )
    } else {
        parse_tile_path(&parts, catalog, task_id, request_id, sla_budget, now)
    }
}

/// Extract `before_layer=<id>` from a query string. Per the static image grammar this is
/// a request-level parameter (applies to all overlays in the URL). Other
/// query parameters are accepted but ignored. Returns `Ok(None)` if not set.
fn parse_before_layer_from_query(query: Option<&str>) -> Result<Option<String>, IngressError> {
    const MAX_LAYER_ID_LEN: usize = 64;
    let Some(q) = query else {
        return Ok(None);
    };
    for pair in q.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "before_layer" {
            continue;
        }
        if value.is_empty() || value.len() > MAX_LAYER_ID_LEN {
            return Err(IngressError::InvalidRequest(
                "before_layer must be 1..=64 characters".to_string(),
            ));
        }
        // Whitelist of style-spec-typical layer-id characters. Keeps mbgl's
        // FFI surface clean and rejects anything that could be smuggled into
        // logs or downstream string interpolation.
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(IngressError::InvalidRequest(
                "before_layer contains an unsupported character".to_string(),
            ));
        }
        return Ok(Some(value.to_string()));
    }
    Ok(None)
}

/// Largest padding value accepted. Pixels — keeps a single side from
/// consuming the whole renderable area for any reasonable image size.
const MAX_PADDING: u16 = 1024;

/// Extract `padding=...` from a query string. Accepts:
/// - `padding=N` — uniform on all sides.
/// - `padding=top,right,bottom,left` — CSS-style 4-value form.
///
/// Other arities (2, 3) are rejected for v0 to avoid the
/// shorthand-ambiguity footgun. Bbox / Auto positioning use this padding
/// when fitting the viewport; `Center` ignores it. Returns `None` when
/// the parameter is absent so positioning-specific defaults can apply.
fn parse_padding_from_query(query: Option<&str>) -> Result<Option<Padding>, IngressError> {
    let Some(q) = query else {
        return Ok(None);
    };
    for pair in q.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "padding" {
            continue;
        }
        return parse_padding_value(value).map(Some);
    }
    Ok(None)
}

fn parse_padding_value(value: &str) -> Result<Padding, IngressError> {
    let parts: Vec<&str> = value.split(',').collect();
    let sides = match parts.as_slice() {
        [v] => {
            let n = parse_padding_side(v, "padding")?;
            [n, n, n, n]
        }
        [t, r, b, l] => [
            parse_padding_side(t, "padding top")?,
            parse_padding_side(r, "padding right")?,
            parse_padding_side(b, "padding bottom")?,
            parse_padding_side(l, "padding left")?,
        ],
        _ => {
            return Err(invalid("padding must be `N` or `top,right,bottom,left`"));
        }
    };
    Ok(Padding {
        top: sides[0],
        right: sides[1],
        bottom: sides[2],
        left: sides[3],
    })
}

fn parse_padding_side(value: &str, name: &str) -> Result<u16, IngressError> {
    let n = value
        .parse::<u16>()
        .map_err(|_| invalid(format!("{name} must be a non-negative integer")))?;
    if n > MAX_PADDING {
        return Err(invalid(format!("{name} must be in [0, {MAX_PADDING}]")));
    }
    Ok(n)
}

fn parse_tile_path(
    parts: &[&str],
    catalog: &StyleCatalog,
    task_id: TaskId,
    request_id: RequestId,
    sla_budget: Duration,
    now: Instant,
) -> Result<InternalTask, IngressError> {
    // 受け付ける形式:
    //   /{style_id}/{z}/{x}/{y}{@scale}.{format}            (4 segments)
    //   /{user}/{style}/{z}/{x}/{y}{@scale}.{format}        (5 segments)
    let (style_id, z_str, x_str, yfmt_str) = match parts {
        [style, z, x, yfmt] => (resolve_style_id(&[*style])?, *z, *x, *yfmt),
        [user, style, z, x, yfmt] => (resolve_style_id(&[*user, *style])?, *z, *x, *yfmt),
        _ => {
            return Err(invalid(
                "tile path must be /{user}/{style}/{z}/{x}/{y}{@scale}.{format}",
            ));
        }
    };
    let style = resolve_style(catalog, style_id)?;
    let z = z_str
        .parse::<u8>()
        .map_err(|_| invalid("tile z must be an integer in 0..=255"))?;
    let x = x_str
        .parse::<u32>()
        .map_err(|_| invalid("tile x must be an integer"))?;
    let (y, scale, output_format) = parse_scale_format(yfmt_str)?;
    validate_tile_coordinate(z, x, y)?;

    Ok(InternalTask {
        id: task_id,
        request_id,
        style: style.revision,
        source: None,
        request: RenderRequest::Tile {
            z,
            x,
            y,
            tile_size: TILE_SIZE,
        },
        pixel_ratio: PixelRatio::from(scale),
        output_format,
        arrived_at: now,
        deadline: now + sla_budget,
        forwarding_hops: 0,
    })
}

/// Largest decoded `addlayer` JSON we accept (bytes, after percent-decode).
/// Keeps a single request from carrying an unbounded style fragment.
const MAX_ADDLAYER_JSON_BYTES: usize = 4096;
/// Maximum object / array nesting depth in `addlayer` JSON. Caps recursive
/// validation cost and protects the style-spec converter from pathological
/// input.
const MAX_ADDLAYER_JSON_DEPTH: usize = 16;
/// Maximum `id` / `source-layer` length in bytes.
const MAX_ADDLAYER_STRING_LEN: usize = 64;
/// `id` namespace reserved for biei-managed layers; users may not place
/// addlayer ids in this prefix.
const ADDLAYER_BIEI_ID_PREFIX: &str = "__biei_";
/// Layer types accepted by the addlayer v0 path. Symbol / background /
/// raster / heatmap / fill-extrusion / hillshade are reserved for later
/// phases that need additional plumbing (icon registry, etc.).
const ADDLAYER_ALLOWED_TYPES: &[&str] = &["fill", "line", "circle"];

/// Extract `addlayer={percent-encoded JSON}` from a query string. At most
/// one `addlayer` parameter is allowed per request (static image API
/// rule). Returns `Ok(None)` if not set.
fn parse_addlayer_from_query(
    query: Option<&str>,
    tileset_catalog: &TilesetCatalog,
) -> Result<Option<AddLayer>, IngressError> {
    let Some(q) = query else {
        return Ok(None);
    };
    let mut found: Option<&str> = None;
    for pair in q.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "addlayer" {
            continue;
        }
        if found.is_some() {
            return Err(invalid(
                "at most one addlayer parameter is allowed per request",
            ));
        }
        found = Some(value);
    }
    let Some(encoded) = found else {
        return Ok(None);
    };
    let decoded = percent_decode_str(encoded)
        .map_err(|_| invalid("addlayer must be valid percent-encoded UTF-8"))?;
    if decoded.is_empty() {
        return Err(invalid("addlayer JSON must not be empty"));
    }
    if decoded.len() > MAX_ADDLAYER_JSON_BYTES {
        return Err(invalid(format!(
            "addlayer JSON must be at most {MAX_ADDLAYER_JSON_BYTES} bytes"
        )));
    }
    let mut value: serde_json::Value = serde_json::from_str(&decoded)
        .map_err(|e| invalid(format!("addlayer JSON parse error: {e}")))?;
    let source = validate_and_rewrite_addlayer_json(&mut value, tileset_catalog)?;
    let json = serde_json::to_string(&value)
        .map_err(|e| invalid(format!("addlayer JSON serialize error: {e}")))?;
    let hash = stable_hash_u64(json.as_bytes());
    Ok(Some(AddLayer { json, hash, source }))
}

fn validate_and_rewrite_addlayer_json(
    value: &mut serde_json::Value,
    tileset_catalog: &TilesetCatalog,
) -> Result<Option<AddLayerSource>, IngressError> {
    check_json_depth(value, MAX_ADDLAYER_JSON_DEPTH)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| invalid("addlayer must be a JSON object"))?;
    // `id` — required, charset, length, no biei-internal prefix.
    let id = obj
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("addlayer requires a string `id`"))?;
    if id.is_empty() || id.len() > MAX_ADDLAYER_STRING_LEN {
        return Err(invalid(format!(
            "addlayer `id` must be 1..={MAX_ADDLAYER_STRING_LEN} bytes"
        )));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid("addlayer `id` may only contain [A-Za-z0-9-_.:]"));
    }
    if id.starts_with(ADDLAYER_BIEI_ID_PREFIX) {
        return Err(invalid(format!(
            "addlayer `id` may not start with the reserved prefix `{ADDLAYER_BIEI_ID_PREFIX}`"
        )));
    }
    // `type` — required, allowed set only.
    let layer_type = obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("addlayer requires a string `type`"))?;
    if !ADDLAYER_ALLOWED_TYPES.contains(&layer_type) {
        return Err(invalid(format!(
            "addlayer `type` must be one of {:?}; got `{layer_type}`",
            ADDLAYER_ALLOWED_TYPES
        )));
    }
    // `source` — required. A string references an existing base-style
    // source. An object is treated as a biei tileset reference: its `url`
    // is a tileset id, not a direct network URL, and is resolved to a
    // request-local source definition before MapLibre sees it.
    let source = obj
        .get("source")
        .ok_or_else(|| invalid("addlayer requires a `source`"))?;
    let rewritten_source = match source {
        serde_json::Value::String(s) => {
            if s.is_empty() || s.len() > MAX_ADDLAYER_STRING_LEN {
                return Err(invalid(format!(
                    "addlayer `source` must be 1..={MAX_ADDLAYER_STRING_LEN} bytes"
                )));
            }
            None
        }
        serde_json::Value::Object(source_obj) => {
            Some(rewrite_addlayer_source_object(source_obj, tileset_catalog)?)
        }
        _ => return Err(invalid("addlayer `source` must be a string or object")),
    };
    // `source-layer` — optional, length only.
    if let Some(sl) = obj.get("source-layer") {
        let sl = sl
            .as_str()
            .ok_or_else(|| invalid("addlayer `source-layer` must be a string"))?;
        if sl.is_empty() || sl.len() > MAX_ADDLAYER_STRING_LEN {
            return Err(invalid(format!(
                "addlayer `source-layer` must be 1..={MAX_ADDLAYER_STRING_LEN} bytes"
            )));
        }
    }
    // `minzoom` / `maxzoom` — optional, 0..=24 (matches positioning zoom).
    for key in ["minzoom", "maxzoom"] {
        if let Some(z) = obj.get(key) {
            let z = z
                .as_f64()
                .ok_or_else(|| invalid(format!("addlayer `{key}` must be a number")))?;
            if !(0.0..=24.0).contains(&z) {
                return Err(invalid(format!("addlayer `{key}` must be in [0, 24]")));
            }
        }
    }
    Ok(rewritten_source)
}

fn rewrite_addlayer_source_object(
    source_obj: &serde_json::Map<String, serde_json::Value>,
    tileset_catalog: &TilesetCatalog,
) -> Result<AddLayerSource, IngressError> {
    let source_type = source_obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("addlayer `source.type` must be a string"))?;
    if source_type != "vector" {
        return Err(invalid(
            "addlayer `source` objects currently support only vector sources",
        ));
    }
    let tileset_id = source_obj
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("addlayer `source.url` must be a tileset id string"))?;
    validate_tileset_id(tileset_id)?;

    let mut resolved = serde_json::Map::new();
    resolved.insert("type".to_string(), serde_json::json!("vector"));
    resolved.insert(
        "url".to_string(),
        serde_json::json!(tileset_catalog.resolve_url(tileset_id)),
    );
    for key in ["minzoom", "maxzoom", "attribution", "bounds", "scheme"] {
        if let Some(value) = source_obj.get(key) {
            resolved.insert(key.to_string(), value.clone());
        }
    }
    let json = serde_json::to_string(&serde_json::Value::Object(resolved))
        .map_err(|e| invalid(format!("addlayer source JSON serialize error: {e}")))?;
    Ok(AddLayerSource {
        tileset_id: tileset_id.to_string(),
        json,
    })
}

fn validate_tileset_id(value: &str) -> Result<(), IngressError> {
    if value.is_empty() || value.len() > MAX_ADDLAYER_STRING_LEN {
        return Err(invalid(format!(
            "addlayer `source.url` tileset id must be 1..={MAX_ADDLAYER_STRING_LEN} bytes"
        )));
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return Err(invalid(
            "addlayer `source.url` must be a biei tileset id, not a direct URL",
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
    {
        return Err(invalid(
            "addlayer `source.url` tileset id contains an unsupported character",
        ));
    }
    Ok(())
}

fn check_json_depth(value: &serde_json::Value, max_depth: usize) -> Result<(), IngressError> {
    fn walk(value: &serde_json::Value, depth: usize, max: usize) -> Result<(), IngressError> {
        if depth > max {
            return Err(invalid(format!(
                "addlayer JSON nesting depth must be at most {max}"
            )));
        }
        match value {
            serde_json::Value::Object(map) => {
                for (_, v) in map {
                    walk(v, depth + 1, max)?;
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, depth + 1, max)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, 0, max_depth)
}

/// Process-local 64-bit hash for cache key separation. Uses Rust's
/// `DefaultHasher`, which has a random per-process seed — so the hash is
/// not stable across processes, but the render output cache is per-node
/// anyway. Two byte-identical inputs in the same process always hash to
/// the same value.
fn stable_hash_u64(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn percent_decode_str(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1).ok_or(())?;
            let lo = *bytes.get(i + 2).ok_or(())?;
            let nibble = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(10 + b - b'a'),
                b'A'..=b'F' => Some(10 + b - b'A'),
                _ => None,
            };
            let byte = nibble(hi)
                .and_then(|h| nibble(lo).map(|l| (h << 4) | l))
                .ok_or(())?;
            out.push(byte);
            i += 3;
        } else if bytes[i] == b'+' {
            // Conventional form-encoded space; tolerate for query-string ergonomics.
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn parse_static_path(
    parts: &[&str],
    before_layer: Option<String>,
    padding: Option<Padding>,
    addlayer: Option<AddLayer>,
    catalog: &StyleCatalog,
    task_id: TaskId,
    request_id: RequestId,
    sla_budget: Duration,
    now: Instant,
) -> Result<InternalTask, IngressError> {
    let (style_id, overlays, position, size_format) = match parts {
        [style_id, "static", position, size_format] => (
            resolve_style_id(&[*style_id])?,
            Vec::new(),
            *position,
            *size_format,
        ),
        [style_id, "static", overlay, position, size_format] => {
            let overlays = parse_static_overlays(overlay)?;
            (
                resolve_style_id(&[*style_id])?,
                overlays,
                *position,
                *size_format,
            )
        }
        [username, style_name, "static", position, size_format] => (
            resolve_style_id(&[*username, *style_name])?,
            Vec::new(),
            *position,
            *size_format,
        ),
        [
            username,
            style_name,
            "static",
            overlay,
            position,
            size_format,
        ] => {
            let overlays = parse_static_overlays(overlay)?;
            (
                resolve_style_id(&[*username, *style_name])?,
                overlays,
                *position,
                *size_format,
            )
        }
        _ => {
            return Err(invalid(
                "static path must be /{style_id}/static/[{overlay}/]{position}/{width}x{height}{@scale}.{format}",
            ));
        }
    };

    let style = resolve_style(catalog, style_id)?;
    let positioning = parse_positioning(position)?;
    let (width, height, scale, output_format) = parse_size_scale_format(size_format)?;
    validate_static_dimensions(width, height, scale)?;
    // `auto` positioning fits the camera to the union of overlay
    // geometries; with zero overlays there is nothing to fit.
    if matches!(positioning, Positioning::Auto) && overlays.is_empty() {
        return Err(invalid(
            "auto positioning requires at least one overlay with geometry",
        ));
    }
    let padding =
        padding.unwrap_or_else(|| default_padding_for_positioning(positioning, width, height));

    Ok(InternalTask {
        id: task_id,
        request_id,
        style: style.revision,
        source: None,
        request: RenderRequest::StaticImage {
            positioning,
            width,
            height,
            overlays,
            before_layer,
            padding,
            addlayer,
        },
        pixel_ratio: PixelRatio::from(scale),
        output_format,
        arrived_at: now,
        deadline: now + sla_budget,
        forwarding_hops: 0,
    })
}

fn default_padding_for_positioning(positioning: Positioning, width: u16, height: u16) -> Padding {
    match positioning {
        Positioning::Auto => Padding {
            top: five_percent_ceil(height),
            right: five_percent_ceil(width),
            bottom: five_percent_ceil(height),
            left: five_percent_ceil(width),
        },
        Positioning::Center { .. } | Positioning::Bbox { .. } => Padding::default(),
    }
}

fn five_percent_ceil(value: u16) -> u16 {
    value.saturating_add(19) / 20
}

struct ResolvedStyle {
    revision: StyleRevision,
}

pub(crate) fn resolve_style_id(components: &[&str]) -> Result<StyleId, IngressError> {
    for component in components {
        validate_path_component(component, "style_id")?;
    }
    Ok(StyleId(components.join("/")))
}

fn resolve_style(catalog: &StyleCatalog, style_id: StyleId) -> Result<ResolvedStyle, IngressError> {
    let Some(version) = catalog.resolve_latest(&style_id) else {
        return Err(IngressError::UnknownStyle(style_id));
    };
    Ok(ResolvedStyle {
        revision: StyleRevision {
            id: style_id,
            version,
        },
    })
}

fn parse_static_overlays(overlay: &str) -> Result<Vec<crate::types::StaticOverlay>, IngressError> {
    crate::http::overlay::parse_static_overlays(overlay)
        .map_err(|err| invalid(format!("invalid static overlay: {err}")))
}

fn validate_path_component(value: &str, name: &str) -> Result<(), IngressError> {
    if value.is_empty() {
        return Err(invalid(format!("{name} must not be empty")));
    }
    if value.contains("..") {
        return Err(invalid(format!("{name} must not contain `..`")));
    }
    Ok(())
}

// Geographic + camera ranges accepted by ingress. Out-of-range values
// would otherwise propagate into MapLibre Native as an uncaught
// `std::domain_error` and crash the process, since the cxx bridge doesn't
// catch C++ exceptions for these paths.
const POSITION_MIN_LON: f64 = -180.0;
const POSITION_MAX_LON: f64 = 180.0;
const POSITION_MIN_LAT: f64 = -90.0;
const POSITION_MAX_LAT: f64 = 90.0;
const POSITION_MIN_ZOOM: f64 = 0.0;
/// Highest zoom MapLibre Native supports. Above this mbgl throws
/// `std::domain_error` from tile coordinate construction.
const POSITION_MAX_ZOOM: f64 = 24.0;
/// mbgl clamps pitch internally but throws on far-out values from the
/// JSON style parser. Pick a value comfortably above the rendered range
/// (~60°) but below anything that would trip native validation.
const POSITION_MAX_PITCH: f32 = 85.0;

fn parse_positioning(value: &str) -> Result<Positioning, IngressError> {
    let decoded;
    let value = if value.as_bytes().contains(&b'%') {
        decoded = percent_decode_str(value)
            .map_err(|_| invalid("position must be valid percent-encoded UTF-8"))?;
        decoded.as_str()
    } else {
        value
    };

    if value == "auto" {
        return Ok(Positioning::Auto);
    }

    if let Some(bbox) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return parse_bbox_positioning(bbox);
    }

    let coords: Vec<_> = value.split(',').collect();
    let (lon, lat, zoom, bearing, pitch) = match coords.as_slice() {
        [lon, lat, zoom] | [lon, lat, zoom, "0"] | [lon, lat, zoom, "0", "0"] => (
            parse_f64(lon, "lon")?,
            parse_f64(lat, "lat")?,
            parse_f64(zoom, "zoom")?,
            0.0_f32,
            0.0_f32,
        ),
        [lon, lat, zoom, bearing] => (
            parse_f64(lon, "lon")?,
            parse_f64(lat, "lat")?,
            parse_f64(zoom, "zoom")?,
            parse_f32(bearing, "bearing")?,
            0.0,
        ),
        [lon, lat, zoom, bearing, pitch] => (
            parse_f64(lon, "lon")?,
            parse_f64(lat, "lat")?,
            parse_f64(zoom, "zoom")?,
            parse_f32(bearing, "bearing")?,
            parse_f32(pitch, "pitch")?,
        ),
        _ => {
            return Err(invalid(
                "position must be auto, [min_lon,min_lat,max_lon,max_lat], or lon,lat,zoom[,bearing[,pitch]]",
            ));
        }
    };

    validate_lon(lon, "lon")?;
    validate_lat(lat, "lat")?;
    validate_zoom(zoom)?;
    validate_pitch(pitch)?;
    Ok(Positioning::Center {
        lon,
        lat,
        zoom,
        bearing,
        pitch,
    })
}

fn parse_bbox_positioning(value: &str) -> Result<Positioning, IngressError> {
    let coords: Vec<_> = value.split(',').collect();
    let [min_lon, min_lat, max_lon, max_lat] = coords.as_slice() else {
        return Err(invalid("bbox must be [min_lon,min_lat,max_lon,max_lat]"));
    };
    let min_lon = parse_f64(min_lon, "min_lon")?;
    let min_lat = parse_f64(min_lat, "min_lat")?;
    let max_lon = parse_f64(max_lon, "max_lon")?;
    let max_lat = parse_f64(max_lat, "max_lat")?;
    validate_lon(min_lon, "min_lon")?;
    validate_lon(max_lon, "max_lon")?;
    validate_lat(min_lat, "min_lat")?;
    validate_lat(max_lat, "max_lat")?;
    if min_lon > max_lon {
        return Err(invalid("bbox min_lon must be <= max_lon"));
    }
    if min_lat > max_lat {
        return Err(invalid("bbox min_lat must be <= max_lat"));
    }
    Ok(Positioning::Bbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

fn validate_lon(value: f64, name: &str) -> Result<(), IngressError> {
    if (POSITION_MIN_LON..=POSITION_MAX_LON).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name} must be in [{POSITION_MIN_LON}, {POSITION_MAX_LON}]"
        )))
    }
}

fn validate_lat(value: f64, name: &str) -> Result<(), IngressError> {
    if (POSITION_MIN_LAT..=POSITION_MAX_LAT).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name} must be in [{POSITION_MIN_LAT}, {POSITION_MAX_LAT}]"
        )))
    }
}

fn validate_zoom(value: f64) -> Result<(), IngressError> {
    if (POSITION_MIN_ZOOM..=POSITION_MAX_ZOOM).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "zoom must be in [{POSITION_MIN_ZOOM}, {POSITION_MAX_ZOOM}]"
        )))
    }
}

fn validate_pitch(value: f32) -> Result<(), IngressError> {
    if (0.0..=POSITION_MAX_PITCH).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "pitch must be in [0, {POSITION_MAX_PITCH}]"
        )))
    }
}

fn parse_size_scale_format(value: &str) -> Result<(u16, u16, Scale, ImageFormat), IngressError> {
    let (size, scale, format) = parse_suffix(value)?;
    let Some((width, height)) = size.split_once('x') else {
        return Err(invalid("static size must be {width}x{height}"));
    };
    let width = width
        .parse::<u16>()
        .map_err(|_| invalid("static width must be an integer"))?;
    let height = height
        .parse::<u16>()
        .map_err(|_| invalid("static height must be an integer"))?;
    Ok((width, height, scale, format))
}

fn parse_scale_format(value: &str) -> Result<(u32, Scale, ImageFormat), IngressError> {
    let (number, scale, format) = parse_suffix(value)?;
    let number = number
        .parse::<u32>()
        .map_err(|_| invalid("tile y must be an integer"))?;
    Ok((number, scale, format))
}

fn parse_suffix(value: &str) -> Result<(&str, Scale, ImageFormat), IngressError> {
    let (body, output_format) = match value.rsplit_once('.') {
        Some((body, "png")) => (body, ImageFormat::Png),
        Some((body, "webp")) => (body, ImageFormat::Webp),
        Some((body, "jpg" | "jpeg")) => (body, ImageFormat::Jpeg),
        Some((_body, _extension)) => return Err(invalid("format must be png, webp, or jpg")),
        None => (value, ImageFormat::Png),
    };
    let (body, scale) = if let Some(body) = body.strip_suffix("@2x") {
        (body, Scale::X2)
    } else if body.contains('@') {
        return Err(invalid("scale must be omitted or @2x"));
    } else {
        (body, Scale::X1)
    };
    Ok((body, scale, output_format))
}

fn validate_tile_coordinate(z: u8, x: u32, y: u32) -> Result<(), IngressError> {
    if z >= 32 {
        return Err(invalid("tile z must be less than 32"));
    }
    let limit = 1_u32 << z;
    if x >= limit || y >= limit {
        return Err(invalid("tile x/y are out of range for z"));
    }
    Ok(())
}

fn validate_static_dimensions(width: u16, height: u16, scale: Scale) -> Result<(), IngressError> {
    if width == 0 || height == 0 {
        return Err(invalid("static width and height must be positive"));
    }
    if width > MAX_STATIC_WIDTH {
        return Err(invalid("static width must be <= 1920"));
    }
    if height > MAX_STATIC_HEIGHT {
        return Err(invalid("static height must be <= 1280"));
    }
    let scale_multiplier = match scale {
        Scale::X1 => 1_u64,
        Scale::X2 => 2,
    };
    let bytes = width as u64 * height as u64 * scale_multiplier * scale_multiplier * 4;
    if bytes > MAX_STATIC_RGBA_BYTES {
        return Err(invalid("static raw RGBA size exceeds limit"));
    }
    Ok(())
}

fn parse_f64(value: &str, name: &str) -> Result<f64, IngressError> {
    value
        .parse::<f64>()
        .map_err(|_| invalid(format!("{name} must be a number")))
}

fn parse_f32(value: &str, name: &str) -> Result<f32, IngressError> {
    value
        .parse::<f32>()
        .map_err(|_| invalid(format!("{name} must be a number")))
}

fn invalid(detail: impl Into<String>) -> IngressError {
    IngressError::InvalidRequest(detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style_catalog::StyleDefinition;

    fn parse_addlayer_from_query(query: Option<&str>) -> Result<Option<AddLayer>, IngressError> {
        super::parse_addlayer_from_query(query, &test_tileset_catalog())
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_path_with_request_id(
        path: &str,
        query: Option<&str>,
        catalog: &StyleCatalog,
        task_id: TaskId,
        request_id: RequestId,
        sla_budget: Duration,
        now: Instant,
    ) -> Result<InternalTask, IngressError> {
        super::parse_path_with_request_id(
            path,
            query,
            catalog,
            &test_tileset_catalog(),
            task_id,
            request_id,
            sla_budget,
            now,
        )
    }

    fn catalog() -> StyleCatalog {
        let catalog = StyleCatalog::new();
        catalog.upsert_definition(
            StyleId("voyager-gl-style".to_string()),
            StyleDefinition::new(
                "https://basemaps.cartocdn.com/gl/voyager-gl-style/style.json",
                1,
            ),
        );
        catalog.upsert_definition(
            StyleId("carto/voyager-gl-style".to_string()),
            StyleDefinition::new(
                "https://basemaps.cartocdn.com/gl/voyager-gl-style/style.json",
                1,
            ),
        );
        catalog
    }

    #[test]
    fn parses_static_path_with_single_segment_style_id() {
        let task = parse_path(
            "/voyager-gl-style/static/none/139.767,35.681,12,0,0/512x384@2x.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static path parses");

        assert_eq!(task.style.id.as_str(), "voyager-gl-style");
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
        assert_eq!(task.output_format, ImageFormat::Png);
        assert!(matches!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Center { .. },
                width: 512,
                height: 384,
                overlays: _,
                ..
            }
        ));
    }

    #[test]
    fn parses_static_without_extension_as_png() {
        let task = parse_path(
            "/voyager-gl-style/static/none/139.767,35.681,12,0,0/512x384@2x",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static path without extension parses");

        assert_eq!(task.output_format, ImageFormat::Png);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
    }

    #[test]
    fn parses_static_jpg() {
        let task = parse_path(
            "/voyager-gl-style/static/none/139.767,35.681,12,0,0/512x384.jpg",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static jpg path parses");

        assert_eq!(task.output_format, ImageFormat::Jpeg);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X1);
    }

    #[test]
    fn parses_static_path_with_bbox_positioning() {
        let task = parse_path(
            "/voyager-gl-style/static/none/[139.7,35.6,139.9,35.8]/512x384.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static bbox path parses");

        assert_eq!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Bbox {
                    min_lon: 139.7,
                    min_lat: 35.6,
                    max_lon: 139.9,
                    max_lat: 35.8,
                },
                width: 512,
                height: 384,
                overlays: Vec::new(),
                before_layer: None,
                padding: Padding::default(),
                addlayer: None,
            }
        );
    }

    #[test]
    fn parses_static_bbox_without_overlay_segment() {
        let task = parse_path(
            "/voyager-gl-style/static/[139.7,35.6,139.9,35.8]/512x384.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static bbox path without overlay parses");

        assert_eq!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Bbox {
                    min_lon: 139.7,
                    min_lat: 35.6,
                    max_lon: 139.9,
                    max_lat: 35.8,
                },
                width: 512,
                height: 384,
                overlays: Vec::new(),
                before_layer: None,
                padding: Padding::default(),
                addlayer: None,
            }
        );
    }

    #[test]
    fn parses_static_bbox_with_percent_encoded_brackets() {
        let task = parse_path(
            "/voyager-gl-style/static/%5B139.7,35.6,139.9,35.8%5D/512x384.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static bbox path with encoded brackets parses");

        assert!(matches!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Bbox {
                    min_lon: 139.7,
                    min_lat: 35.6,
                    max_lon: 139.9,
                    max_lat: 35.8,
                },
                ..
            }
        ));
    }

    #[test]
    fn parses_static_center_png_2x() {
        let now = Instant::now();
        let task = parse_path(
            "/carto/voyager-gl-style/static/139.767,35.681,12,0,0/512x384@2x.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            now,
        )
        .expect("static path parses");

        assert_eq!(task.id, 42);
        assert_eq!(task.style.id.as_str(), "carto/voyager-gl-style");
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
        assert_eq!(task.output_format, ImageFormat::Png);
        assert_eq!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Center {
                    lon: 139.767,
                    lat: 35.681,
                    zoom: 12.0,
                    bearing: 0.0,
                    pitch: 0.0,
                },
                width: 512,
                height: 384,
                overlays: Vec::new(),
                before_layer: None,
                padding: Padding::default(),
                addlayer: None,
            }
        );
        assert_eq!(task.arrived_at, now);
        assert_eq!(task.deadline, now + Duration::from_secs(30));
    }

    #[test]
    fn parses_padding_uniform_from_query() {
        assert_eq!(
            parse_padding_from_query(Some("padding=20")).expect("uniform"),
            Some(Padding::all(20))
        );
    }

    #[test]
    fn parses_padding_four_sides_from_query() {
        assert_eq!(
            parse_padding_from_query(Some("padding=1,2,3,4")).expect("4 sides"),
            Some(Padding {
                top: 1,
                right: 2,
                bottom: 3,
                left: 4,
            })
        );
    }

    #[test]
    fn padding_returns_none_when_absent() {
        assert_eq!(parse_padding_from_query(None).unwrap(), None);
        assert_eq!(parse_padding_from_query(Some("foo=bar")).unwrap(), None);
    }

    #[test]
    fn rejects_padding_with_unsupported_arity() {
        let err = parse_padding_from_query(Some("padding=10,20")).expect_err("2-value rejected");
        assert!(err.to_string().contains("padding"));
        let err = parse_padding_from_query(Some("padding=10,20,30")).expect_err("3-value rejected");
        assert!(err.to_string().contains("padding"));
    }

    #[test]
    fn rejects_padding_above_max() {
        let err = parse_padding_from_query(Some("padding=99999"))
            .expect_err("padding above MAX_PADDING rejected");
        assert!(err.to_string().contains("padding"));
    }

    fn encode_addlayer(json: &str) -> String {
        // Percent-encode the characters that would otherwise break the
        // outer query-string (`%`, `&`, `=`, `+`, `#`). The tests below
        // exercise both compact and expanded JSON via this helper.
        let mut out = String::new();
        for b in json.as_bytes() {
            match *b {
                b'%' => out.push_str("%25"),
                b'&' => out.push_str("%26"),
                b'=' => out.push_str("%3D"),
                b'+' => out.push_str("%2B"),
                b'#' => out.push_str("%23"),
                b' ' => out.push_str("%20"),
                _ => out.push(*b as char),
            }
        }
        out
    }

    fn addlayer_query(json: &str) -> String {
        format!("addlayer={}", encode_addlayer(json))
    }

    #[test]
    fn parses_valid_addlayer_from_query() {
        let json = r##"{"id":"my-fill","type":"fill","source":"composite","paint":{"fill-color":"#ff0000"}}"##;
        let layer = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .expect("valid addlayer parses")
            .expect("layer present");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&layer.json).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
        // Same JSON should hash to the same value within a process.
        let again = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .unwrap()
            .unwrap();
        assert_eq!(layer.hash, again.hash);
    }

    #[test]
    fn addlayer_absent_returns_none() {
        assert!(parse_addlayer_from_query(None).unwrap().is_none());
        assert!(
            parse_addlayer_from_query(Some("padding=10"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_multiple_addlayer_params() {
        let json = r#"{"id":"x","type":"fill","source":"s"}"#;
        let q = format!("{}&{}", addlayer_query(json), addlayer_query(json));
        let err = parse_addlayer_from_query(Some(&q)).expect_err("multiple addlayer rejected");
        assert!(err.to_string().contains("at most one"));
    }

    #[test]
    fn rejects_oversize_addlayer_json() {
        // Pad the paint object so the decoded JSON exceeds MAX_ADDLAYER_JSON_BYTES.
        let big = "x".repeat(MAX_ADDLAYER_JSON_BYTES + 100);
        let json = format!(r#"{{"id":"x","type":"fill","source":"s","metadata":"{big}"}}"#);
        let err = parse_addlayer_from_query(Some(&addlayer_query(&json)))
            .expect_err("oversize addlayer rejected");
        assert!(err.to_string().contains("at most"));
    }

    #[test]
    fn rejects_deeply_nested_addlayer_json() {
        // Build a deeply-nested array inside `paint.filter`.
        let mut nested = String::from(r#"{"id":"x","type":"fill","source":"s","paint":{"a":"#);
        let depth = MAX_ADDLAYER_JSON_DEPTH + 5;
        for _ in 0..depth {
            nested.push('[');
        }
        nested.push('0');
        for _ in 0..depth {
            nested.push(']');
        }
        nested.push_str("}}");
        let err = parse_addlayer_from_query(Some(&addlayer_query(&nested)))
            .expect_err("nesting depth rejected");
        assert!(err.to_string().contains("nesting"));
    }

    #[test]
    fn rejects_addlayer_disallowed_type() {
        for ty in [
            "background",
            "raster",
            "heatmap",
            "fill-extrusion",
            "symbol",
        ] {
            let json = format!(r#"{{"id":"x","type":"{ty}","source":"s"}}"#);
            assert!(
                parse_addlayer_from_query(Some(&addlayer_query(&json))).is_err(),
                "type `{ty}` should be rejected"
            );
        }
    }

    #[test]
    fn addlayer_source_url_is_resolved_to_tileset_json_url() {
        let json = r#"{"id":"x","type":"fill","source":{"type":"vector","url":"weather-tiles"}}"#;
        let layer = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .expect("addlayer parses")
            .expect("layer present");
        let source = layer.source.expect("source object is carried separately");
        assert_eq!(source.tileset_id, "weather-tiles");
        let source_json: serde_json::Value =
            serde_json::from_str(&source.json).expect("source JSON");
        assert_eq!(source_json["type"], serde_json::json!("vector"));
        assert_eq!(
            source_json["url"],
            serde_json::json!("https://tiles.example.test/weather-tiles/tileset.json")
        );
        let layer_json: serde_json::Value = serde_json::from_str(&layer.json).expect("layer JSON");
        assert!(layer_json["source"].is_object());
    }

    #[test]
    fn rejects_addlayer_source_url_direct_network_url() {
        let json = r#"{"id":"x","type":"fill","source":{"type":"vector","url":"https://example.test/tiles.json"}}"#;
        let err = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .expect_err("direct source URL rejected");
        assert!(err.to_string().contains("not a direct URL"));
    }

    #[test]
    fn rejects_addlayer_id_with_biei_prefix() {
        let json = r#"{"id":"__biei_user","type":"fill","source":"s"}"#;
        let err = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .expect_err("biei prefix rejected");
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn rejects_addlayer_id_with_bad_charset() {
        let json = r#"{"id":"my fill","type":"fill","source":"s"}"#;
        let err = parse_addlayer_from_query(Some(&addlayer_query(json)))
            .expect_err("space in id rejected");
        assert!(err.to_string().contains("id"));
    }

    #[test]
    fn rejects_addlayer_with_missing_required_fields() {
        // missing id
        assert!(
            parse_addlayer_from_query(Some(&addlayer_query(r#"{"type":"fill","source":"s"}"#)))
                .is_err()
        );
        // missing type
        assert!(
            parse_addlayer_from_query(Some(&addlayer_query(r#"{"id":"x","source":"s"}"#))).is_err()
        );
        // missing source
        assert!(
            parse_addlayer_from_query(Some(&addlayer_query(r#"{"id":"x","type":"fill"}"#)))
                .is_err()
        );
    }

    #[test]
    fn rejects_addlayer_with_out_of_range_zoom() {
        let json = r#"{"id":"x","type":"fill","source":"s","minzoom":-1}"#;
        assert!(parse_addlayer_from_query(Some(&addlayer_query(json))).is_err());
        let json = r#"{"id":"x","type":"fill","source":"s","maxzoom":25}"#;
        assert!(parse_addlayer_from_query(Some(&addlayer_query(json))).is_err());
    }

    #[test]
    fn parse_path_threads_addlayer_into_render_request() {
        let json = r#"{"id":"my-line","type":"line","source":"composite"}"#;
        let task = parse_path_with_request_id(
            "/voyager-gl-style/static/none/139.7,35.6,12/256x256.webp",
            Some(&addlayer_query(json)),
            &catalog(),
            1,
            RequestId::default(),
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("addlayer threads through ingress");
        if let RenderRequest::StaticImage { addlayer, .. } = task.request {
            let a = addlayer.expect("addlayer present");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&a.json).unwrap(),
                serde_json::from_str::<serde_json::Value>(json).unwrap()
            );
        } else {
            panic!("expected StaticImage");
        }
    }

    #[test]
    fn parses_auto_positioning_with_overlay() {
        let task = parse_path(
            "/voyager-gl-style/static/path-2+f44(_p~iF~ps|U_ulLnnqC_mqNvxq%60@)/auto/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("auto with one overlay parses");
        let request = task.request;
        assert!(matches!(
            request,
            RenderRequest::StaticImage {
                positioning: Positioning::Auto,
                padding: Padding {
                    top: 20,
                    right: 26,
                    bottom: 20,
                    left: 26,
                },
                ..
            }
        ));
    }

    #[test]
    fn parses_auto_positioning_with_padding_query() {
        // `?padding=40` lives in the query string, which parse_path splits
        // out at the HTTP layer; use parse_path_with_request_id directly to
        // exercise the query-parameter path.
        let task = parse_path_with_request_id(
            "/voyager-gl-style/static/path-2+f44(_p~iF~ps|U_ulLnnqC_mqNvxq%60@)/auto/512x384.png",
            Some("padding=40"),
            &catalog(),
            1,
            RequestId::default(),
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("auto+padding parses");
        if let RenderRequest::StaticImage { padding, .. } = task.request {
            assert_eq!(padding, Padding::all(40));
        } else {
            panic!("expected StaticImage");
        }
    }

    #[test]
    fn rejects_auto_positioning_without_overlays() {
        // `auto` fits the camera to overlay geometries; if there are no
        // overlays there is nothing to fit, so reject up front instead of
        // letting it propagate to the renderer.
        let err = parse_path(
            "/carto/voyager-gl-style/static/none/auto/256x256.webp",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("auto with no overlays must be rejected");
        assert!(err.to_string().contains("auto positioning"));
    }

    #[test]
    fn parses_static_with_none_overlay() {
        // `none` overlay segment + a bbox / center positioning is valid;
        // overlays are simply empty.
        let task = parse_path(
            "/carto/voyager-gl-style/static/none/139.7,35.6,12/256x256.webp",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static path parses");

        assert_eq!(task.output_format, ImageFormat::Webp);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X1);
        assert!(matches!(
            task.request,
            RenderRequest::StaticImage {
                positioning: Positioning::Center { .. },
                overlays: ref o,
                ..
            } if o.is_empty()
        ));
    }

    #[test]
    fn parses_static_path_overlay() {
        let task = parse_path(
            "/voyager-gl-style/static/path-5+f44-0.5(_p~iF~ps%7CU)/139.767,35.681,12/256x256.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("path overlay parses");

        let RenderRequest::StaticImage { overlays, .. } = task.request else {
            panic!("expected static image request");
        };
        assert_eq!(overlays.len(), 1);
    }

    #[test]
    fn parses_static_pin_overlay() {
        let task = parse_path(
            "/voyager-gl-style/static/pin-s-a+9ed4bd(139,35)/auto/256x256.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("pin overlay parses");

        let RenderRequest::StaticImage { overlays, .. } = task.request else {
            panic!("expected static image request");
        };
        assert_eq!(overlays.len(), 1);
    }

    #[test]
    fn parses_tile_webp_2x() {
        let task = parse_path(
            "/carto/voyager-gl-style/1/1/0@2x.webp",
            &catalog(),
            7,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("tile path parses");

        assert_eq!(task.output_format, ImageFormat::Webp);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
        assert_eq!(
            task.request,
            RenderRequest::Tile {
                z: 1,
                x: 1,
                y: 0,
                tile_size: TILE_SIZE,
            }
        );
    }

    #[test]
    fn parses_tile_without_extension_as_png() {
        let task = parse_path(
            "/carto/voyager-gl-style/1/1/0@2x",
            &catalog(),
            7,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("tile path without extension parses");

        assert_eq!(task.output_format, ImageFormat::Png);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
        assert_eq!(
            task.request,
            RenderRequest::Tile {
                z: 1,
                x: 1,
                y: 0,
                tile_size: TILE_SIZE,
            }
        );
    }

    #[test]
    fn parses_tile_jpeg_alias() {
        let task = parse_path(
            "/carto/voyager-gl-style/1/1/0.jpeg",
            &catalog(),
            7,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("tile jpeg path parses");

        assert_eq!(task.output_format, ImageFormat::Jpeg);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X1);
    }

    #[test]
    fn rejects_unknown_style() {
        let err = parse_path(
            "/carto/unknown/static/auto/256x256.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("style is unknown");

        assert!(matches!(err, IngressError::UnknownStyle(_)));
    }

    #[test]
    fn rejects_invalid_format_and_scale() {
        let err = parse_path(
            "/carto/voyager-gl-style/static/auto/256x256.gif",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("format is invalid");

        assert!(err.to_string().contains("format"));
    }

    #[test]
    fn rejects_static_dimension_over_limit() {
        let err = parse_path(
            "/carto/voyager-gl-style/static/auto/1921x256.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("dimension is too large");

        assert!(err.to_string().contains("<= 1920"));
    }

    #[test]
    fn rejects_static_lat_out_of_range() {
        // Regression: `130,140,30,35` parsed as lon=130, lat=140, zoom=30,
        // bearing=35 and passed straight into mbgl, which threw an uncaught
        // std::domain_error and aborted the process.
        let err = parse_path(
            "/voyager-gl-style/static/130,140,12,0/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("lat above 90 must be rejected");
        assert!(err.to_string().contains("lat"));
    }

    #[test]
    fn rejects_static_lon_out_of_range() {
        let err = parse_path(
            "/voyager-gl-style/static/250,35,12,0/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("lon above 180 must be rejected");
        assert!(err.to_string().contains("lon"));
    }

    #[test]
    fn rejects_static_zoom_out_of_range() {
        let err = parse_path(
            "/voyager-gl-style/static/139.7,35.6,30/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("zoom above 24 must be rejected");
        assert!(err.to_string().contains("zoom"));
    }

    #[test]
    fn rejects_static_pitch_out_of_range() {
        let err = parse_path(
            "/voyager-gl-style/static/139.7,35.6,12,0,90/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("pitch above 85 must be rejected");
        assert!(err.to_string().contains("pitch"));
    }

    #[test]
    fn rejects_static_bbox_with_lat_out_of_range() {
        let err = parse_path(
            "/voyager-gl-style/static/[139.7,-95,139.9,35.8]/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("bbox min_lat below -90 must be rejected");
        assert!(err.to_string().contains("min_lat"));
    }

    #[test]
    fn rejects_static_bbox_with_swapped_bounds() {
        let err = parse_path(
            "/voyager-gl-style/static/[140.0,35.6,139.7,35.8]/512x384.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("bbox min_lon > max_lon must be rejected");
        assert!(err.to_string().contains("min_lon"));
    }

    #[test]
    fn rejects_tile_coordinates_out_of_range() {
        let err = parse_path(
            "/carto/voyager-gl-style/1/2/0.png",
            &catalog(),
            1,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect_err("x is out of range for z");

        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn parses_tile_with_single_segment_style_id() {
        let task = parse_path(
            "/voyager-gl-style/2/3/1@2x.webp",
            &catalog(),
            8,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("single-segment style tile path parses");

        assert_eq!(task.style.id.as_str(), "voyager-gl-style");
        assert_eq!(task.output_format, ImageFormat::Webp);
        assert_eq!(task.pixel_ratio.to_scale(), Scale::X2);
        assert!(matches!(task.request, RenderRequest::Tile { z: 2, .. }));
    }

    #[test]
    fn maps_ingress_concurrency_limit_to_retryable_503() {
        let response = IngressResponse::json(503, "ingress_busy", "").with_retry_after("1");

        assert_eq!(response.status, 503);
        assert_eq!(response.headers, vec![("Retry-After", "1".to_string())]);
        assert!(
            std::str::from_utf8(&response.body)
                .expect("json body")
                .contains("ingress_busy")
        );
    }

    #[test]
    fn maps_ingress_drain_to_service_draining_label() {
        let response = IngressResponse::json(503, "service_draining", "").with_retry_after("2");

        assert_eq!(response.status, 503);
        assert_eq!(response.headers, vec![("Retry-After", "2".to_string())]);
        assert!(
            std::str::from_utf8(&response.body)
                .expect("json body")
                .contains("service_draining")
        );
    }

    #[test]
    fn parses_before_layer_from_query_when_present() {
        let parsed =
            parse_before_layer_from_query(Some("before_layer=road-label")).expect("valid layer id");
        assert_eq!(parsed.as_deref(), Some("road-label"));
    }

    #[test]
    fn parse_before_layer_returns_none_for_absent_or_unrelated_query() {
        assert_eq!(parse_before_layer_from_query(None), Ok(None));
        assert_eq!(parse_before_layer_from_query(Some("")), Ok(None));
        assert_eq!(parse_before_layer_from_query(Some("foo=bar")), Ok(None));
    }

    #[test]
    fn parse_before_layer_rejects_invalid_characters() {
        // `/` is reserved for layer-id-like inputs; reject so it can't smuggle
        // path segments or weird tokens into mbgl's add_layer_before.
        let err = parse_before_layer_from_query(Some("before_layer=foo/bar"))
            .expect_err("slash is not in the whitelist");
        assert!(matches!(err, IngressError::InvalidRequest(_)));
    }

    #[test]
    fn parse_before_layer_rejects_overly_long_value() {
        let long = format!("before_layer={}", "a".repeat(65));
        assert!(matches!(
            parse_before_layer_from_query(Some(&long)),
            Err(IngressError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_path_threads_before_layer_into_render_request() {
        let task = parse_path_with_request_id(
            "/voyager-gl-style/static/-122,37,9/512x384.png",
            Some("before_layer=labels"),
            &catalog(),
            42,
            RequestId::from_string("test"),
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static URL parses");

        match task.request {
            RenderRequest::StaticImage { before_layer, .. } => {
                assert_eq!(before_layer.as_deref(), Some("labels"));
            }
            _ => panic!("expected StaticImage"),
        }
    }

    #[test]
    fn parse_path_defaults_before_layer_to_none_when_query_absent() {
        let task = parse_path(
            "/voyager-gl-style/static/-122,37,9/512x384.png",
            &catalog(),
            42,
            Duration::from_secs(30),
            Instant::now(),
        )
        .expect("static URL parses");

        match task.request {
            RenderRequest::StaticImage { before_layer, .. } => assert!(before_layer.is_none()),
            _ => panic!("expected StaticImage"),
        }
    }
}
