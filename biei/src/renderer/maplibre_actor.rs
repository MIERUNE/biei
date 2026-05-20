//! Dedicated blocking renderer actor for production MapLibre integration.
//!
//! MapLibre Native rendering is treated as thread-affine blocking work. This
//! actor owns the backend on one OS thread and exposes async request/reply
//! methods to worker tasks.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use tokio::sync::oneshot;
use tokio::time::Instant;

use super::maplibre_overlay::{
    OverlaySlotPool, build_overlay_geojson, pin_auto_padding_inset, populate_static_slots,
    render_static_with_overlays,
};

use crate::types::{
    ImageFormat, InternalTask, Padding, PixelRatio, RenderOutput, RenderRequest, RendererError,
    SourceRef, StaticOverlay, StyleRevision, TaskId, WorkerId,
};

/// Wrap `render_static_with_overlays` with optional request-local
/// `addlayer` install / remove. The addlayer is inserted before overlay
/// slots reposition themselves, so the slot pool's later
/// `assign_slots`/`move_all_layers` finds it in the style and the
/// overlays end up above it within the same Z band.
///
/// On any exit path (success or failure) the addlayer layer is removed
/// before returning, since its id is derived from `task_id` and reusing it
/// on the next request would collide with the lingering installation.
/// Request-local sources use stable ids and are kept in a small worker-local
/// cache; without a referencing layer they do not draw anything.
#[allow(clippy::too_many_arguments)]
fn render_static_with_overlays_and_addlayer(
    renderer: &mut maplibre_native::ImageRenderer<maplibre_native::Static>,
    slots: &mut OverlaySlotPool,
    addlayer_sources: &mut AddLayerSourceCache,
    camera: &maplibre_native::CameraUpdate,
    overlays: &[StaticOverlay],
    before_layer: Option<&str>,
    addlayer: Option<&crate::types::AddLayer>,
    task_id: crate::types::TaskId,
) -> Result<maplibre_native::Image, maplibre_native::RenderingError> {
    let installed_addlayer = if let Some(layer) = addlayer {
        let mut style = renderer.style();
        match install_addlayer(&mut style, addlayer_sources, layer, before_layer, task_id) {
            Ok(installed) => Some(installed),
            Err(e) => return Err(maplibre_native::RenderingError::Native(e.to_string())),
        }
    } else {
        None
    };
    let result = render_static_with_overlays(renderer, slots, camera, overlays, before_layer);
    if let Some(installed) = &installed_addlayer {
        let mut style = renderer.style();
        remove_addlayer(&mut style, installed);
    }
    result
}

struct InstalledAddLayer {
    layer_id: String,
}

const ADDLAYER_SOURCE_CACHE_CAPACITY: usize = 64;

struct AddLayerSourceCache {
    ids: HashSet<String>,
    lru: VecDeque<String>,
    capacity: usize,
}

impl AddLayerSourceCache {
    fn new() -> Self {
        Self {
            ids: HashSet::new(),
            lru: VecDeque::new(),
            capacity: ADDLAYER_SOURCE_CACHE_CAPACITY,
        }
    }

    fn ensure(
        &mut self,
        style: &mut maplibre_native::StyleRef<'_, maplibre_native::Static>,
        source: &crate::types::AddLayerSource,
    ) -> Result<(String, bool), maplibre_native::StyleError> {
        let id = source.stable_source_id();
        if self.ids.contains(&id) {
            self.touch(&id);
            return Ok((id, false));
        }
        while self.ids.len() >= self.capacity
            && let Some(evicted) = self.lru.pop_front()
        {
            self.ids.remove(&evicted);
            style.remove_source(&evicted);
        }
        let any_source = maplibre_native::AnySource::from_json_str(&id, &source.json)?;
        style.add_source(any_source)?;
        self.ids.insert(id.clone());
        self.lru.push_back(id.clone());
        Ok((id, true))
    }

    fn remove_if_present(
        &mut self,
        style: &mut maplibre_native::StyleRef<'_, maplibre_native::Static>,
        id: &str,
    ) {
        if !self.ids.remove(id) {
            return;
        }
        self.lru.retain(|cached| cached != id);
        style.remove_source(id);
    }

    fn touch(&mut self, id: &str) {
        self.lru.retain(|cached| cached != id);
        self.lru.push_back(id.to_string());
    }
}

/// Install the request-local `addlayer` onto the active style and return
/// the biei-internal layer id that needs to be removed after rendering.
/// The user-supplied `id` from the addlayer JSON is rewritten to a
/// `__biei_addlayer_{task_id}` namespace to keep biei-managed layers
/// distinct from arbitrary user-supplied ids — mbgl's style throws on
/// duplicate ids, so this also prevents accidental collision with the
/// base style or with previously-installed biei slots.
///
/// Placement: addlayer sits at the bottom of the biei-managed band. When
/// `before_layer={X}` is set, the layer is inserted before X (matching
/// `before_layer`'s semantics for overlays). Otherwise it lands at the
/// top of the base style, where the overlay slot pool will later add its
/// own layers above it.
fn install_addlayer(
    style: &mut maplibre_native::StyleRef<'_, maplibre_native::Static>,
    addlayer_sources: &mut AddLayerSourceCache,
    addlayer: &crate::types::AddLayer,
    before_layer: Option<&str>,
    task_id: crate::types::TaskId,
) -> Result<InstalledAddLayer, maplibre_native::StyleError> {
    let internal_id = format!("__biei_addlayer_{task_id}");
    let mut newly_added_source_id = None;
    let source_id = match &addlayer.source {
        Some(source) => {
            let (id, newly_added) = addlayer_sources.ensure(style, source)?;
            if newly_added {
                newly_added_source_id = Some(id.clone());
            }
            Some(id)
        }
        None => None,
    };
    let rewritten =
        match rewrite_addlayer_id_and_source(&addlayer.json, &internal_id, source_id.as_deref()) {
            Ok(rewritten) => rewritten,
            Err(err) => {
                if let Some(source_id) = &newly_added_source_id {
                    addlayer_sources.remove_if_present(style, source_id);
                }
                return Err(err);
            }
        };
    let layer = match maplibre_native::AnyLayer::from_json_str(&rewritten) {
        Ok(layer) => layer,
        Err(err) => {
            if let Some(source_id) = &newly_added_source_id {
                addlayer_sources.remove_if_present(style, source_id);
            }
            return Err(err);
        }
    };
    let added_layer = match before_layer {
        Some(b) => style.add_layer_before(layer, b),
        None => style.add_layer(layer),
    };
    if let Err(err) = added_layer {
        if let Some(source_id) = &newly_added_source_id {
            addlayer_sources.remove_if_present(style, source_id);
        }
        return Err(err);
    }
    Ok(InstalledAddLayer {
        layer_id: internal_id,
    })
}

/// Drop a previously-installed addlayer layer. Best-effort: a failed remove
/// would leave the layer in the style for the next request, where the same
/// `task_id`-derived id would collide. Stable addlayer sources are intentionally
/// left cached; without this layer they are unreferenced and invisible.
fn remove_addlayer(
    style: &mut maplibre_native::StyleRef<'_, maplibre_native::Static>,
    installed: &InstalledAddLayer,
) {
    let _ = style.remove_layer(&installed.layer_id);
}

/// Rewrite the `id` field of a style-spec layer JSON to `new_id`. The
/// input has already been validated at ingress, so we expect a JSON
/// object; we still return `Err` instead of panicking on a misshape so
/// that the `Result` plumbing handles any drift from validation.
fn rewrite_addlayer_id_and_source(
    json: &str,
    new_id: &str,
    new_source_id: Option<&str>,
) -> Result<String, maplibre_native::StyleError> {
    let mut value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| maplibre_native::StyleError::Native(format!("addlayer JSON: {e}")))?;
    let obj = value.as_object_mut().ok_or_else(|| {
        maplibre_native::StyleError::Native("addlayer JSON must be an object".to_string())
    })?;
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(new_id.to_string()),
    );
    if let Some(source_id) = new_source_id {
        obj.insert(
            "source".to_string(),
            serde_json::Value::String(source_id.to_string()),
        );
    }
    serde_json::to_string(&value)
        .map_err(|e| maplibre_native::StyleError::Native(format!("addlayer reserialize: {e}")))
}

/// Convert URL-pixel padding (biei) to mbgl `EdgeInsets` (logical pixels,
/// f64). The two share the same coordinate system for static rendering,
/// so this is a straight cast.
fn padding_to_edge_insets(p: Padding) -> maplibre_native::EdgeInsets {
    maplibre_native::EdgeInsets {
        top: f64::from(p.top),
        right: f64::from(p.right),
        bottom: f64::from(p.bottom),
        left: f64::from(p.left),
    }
}

fn auto_padding_for_overlays(
    mut padding: Padding,
    overlays: &[StaticOverlay],
    width: u16,
    height: u16,
) -> Padding {
    let Some(bounds) = overlay_bounds(overlays) else {
        return padding;
    };
    let pin_inset = overlays.iter().fold(Padding::default(), |acc, overlay| {
        let StaticOverlay::Pin(pin) = overlay else {
            return acc;
        };
        let inset = pin_auto_padding_inset(pin.size);
        let deficit = pin_padding_deficit(padding, bounds, pin.coordinate, inset, width, height);
        Padding {
            top: acc.top.max(deficit.top),
            right: acc.right.max(deficit.right),
            bottom: acc.bottom.max(deficit.bottom),
            left: acc.left.max(deficit.left),
        }
    });
    padding.top = padding.top.saturating_add(pin_inset.top);
    padding.right = padding.right.saturating_add(pin_inset.right);
    padding.bottom = padding.bottom.saturating_add(pin_inset.bottom);
    padding.left = padding.left.saturating_add(pin_inset.left);
    padding
}

fn pin_padding_deficit(
    padding: Padding,
    bounds: OverlayBounds,
    point: crate::types::LngLat,
    inset: Padding,
    width: u16,
    height: u16,
) -> Padding {
    let clearance = projected_pin_clearance(padding, bounds, point, width, height);
    Padding {
        top: missing_padding(inset.top, clearance.top),
        right: missing_padding(inset.right, clearance.right),
        bottom: missing_padding(inset.bottom, clearance.bottom),
        left: missing_padding(inset.left, clearance.left),
    }
}

#[derive(Clone, Copy, Debug)]
struct EdgeClearance {
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
}

fn projected_pin_clearance(
    padding: Padding,
    bounds: OverlayBounds,
    point: crate::types::LngLat,
    width: u16,
    height: u16,
) -> EdgeClearance {
    let inner_width = f64::from(
        u32::from(width)
            .saturating_sub(u32::from(padding.left) + u32::from(padding.right))
            .max(1),
    );
    let inner_height = f64::from(
        u32::from(height)
            .saturating_sub(u32::from(padding.top) + u32::from(padding.bottom))
            .max(1),
    );

    let min_x = mercator_x(bounds.min_lon);
    let max_x = mercator_x(bounds.max_lon);
    let point_x = mercator_x(point.lon);
    let x_span = (max_x - min_x).max(0.0);
    let (min_y, max_y, point_y) = mercator_coordinates(bounds, point);
    let y_span = (max_y - min_y).max(0.0);
    let scale = projected_fit_scale(x_span, y_span, inner_width, inner_height);
    let horizontal_slack = ((inner_width - x_span * scale) / 2.0).max(0.0);
    let vertical_slack = ((inner_height - y_span * scale) / 2.0).max(0.0);

    EdgeClearance {
        top: f64::from(padding.top) + vertical_slack + ((max_y - point_y).max(0.0) * scale),
        right: f64::from(padding.right) + horizontal_slack + ((max_x - point_x).max(0.0) * scale),
        bottom: f64::from(padding.bottom) + vertical_slack + ((point_y - min_y).max(0.0) * scale),
        left: f64::from(padding.left) + horizontal_slack + ((point_x - min_x).max(0.0) * scale),
    }
}

fn projected_fit_scale(x_span: f64, y_span: f64, inner_width: f64, inner_height: f64) -> f64 {
    match (x_span > f64::EPSILON, y_span > f64::EPSILON) {
        (true, true) => (inner_width / x_span).min(inner_height / y_span),
        (true, false) => inner_width / x_span,
        (false, true) => inner_height / y_span,
        (false, false) => 1.0,
    }
}

fn mercator_coordinates(bounds: OverlayBounds, point: crate::types::LngLat) -> (f64, f64, f64) {
    (
        mercator_y(bounds.min_lat),
        mercator_y(bounds.max_lat),
        mercator_y(point.lat),
    )
}

fn mercator_x(lon: f64) -> f64 {
    let lon = if lon.is_finite() { lon } else { 0.0 };
    lon.to_radians()
}

fn missing_padding(required: u16, available: f64) -> u16 {
    let available = available.max(0.0).floor() as u16;
    required.saturating_sub(available)
}

fn mercator_y(lat: f64) -> f64 {
    let lat = if lat.is_finite() { lat } else { 0.0 };
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()
}

#[derive(Clone, Copy, Debug)]
struct OverlayBounds {
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
}

impl OverlayBounds {
    fn new(point: crate::types::LngLat) -> Self {
        Self {
            min_lon: point.lon,
            min_lat: point.lat,
            max_lon: point.lon,
            max_lat: point.lat,
        }
    }

    fn include(&mut self, point: crate::types::LngLat) {
        self.min_lon = self.min_lon.min(point.lon);
        self.min_lat = self.min_lat.min(point.lat);
        self.max_lon = self.max_lon.max(point.lon);
        self.max_lat = self.max_lat.max(point.lat);
    }
}

fn overlay_bounds(overlays: &[StaticOverlay]) -> Option<OverlayBounds> {
    let mut bounds = None;
    for overlay in overlays {
        match overlay {
            StaticOverlay::Path(path) => {
                for point in &path.coordinates {
                    include_bounds(&mut bounds, *point);
                }
            }
            StaticOverlay::Pin(pin) => include_bounds(&mut bounds, pin.coordinate),
            StaticOverlay::GeoJson(geojson) => {
                collect_geojson_bounds(&geojson.feature_collection, &mut bounds);
            }
        }
    }
    bounds
}

fn include_bounds(bounds: &mut Option<OverlayBounds>, point: crate::types::LngLat) {
    match bounds {
        Some(bounds) => bounds.include(point),
        None => *bounds = Some(OverlayBounds::new(point)),
    }
}

fn collect_geojson_bounds(value: &serde_json::Value, bounds: &mut Option<OverlayBounds>) {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("FeatureCollection") => {
            if let Some(features) = value.get("features").and_then(serde_json::Value::as_array) {
                for feature in features {
                    collect_geojson_bounds(feature, bounds);
                }
            }
        }
        Some("Feature") => {
            if let Some(geometry) = value.get("geometry") {
                collect_geojson_bounds(geometry, bounds);
            }
        }
        Some("Point")
        | Some("MultiPoint")
        | Some("LineString")
        | Some("MultiLineString")
        | Some("Polygon")
        | Some("MultiPolygon") => collect_geojson_coordinates(value.get("coordinates"), bounds),
        _ => {}
    }
}

fn collect_geojson_coordinates(
    value: Option<&serde_json::Value>,
    bounds: &mut Option<OverlayBounds>,
) {
    let Some(value) = value else {
        return;
    };
    let Some(array) = value.as_array() else {
        return;
    };
    if array.len() >= 2
        && array[0].is_number()
        && array[1].is_number()
        && let (Some(lon), Some(lat)) = (array[0].as_f64(), array[1].as_f64())
    {
        include_bounds(bounds, crate::types::LngLat { lon, lat });
        return;
    }
    for item in array {
        collect_geojson_coordinates(Some(item), bounds);
    }
}

const ACTOR_JOIN_GRACE: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Clone, Debug)]
pub struct RendererActorConfig {
    pub worker_id: WorkerId,
    pub ambient_cache_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ResolvedStyle {
    pub revision: StyleRevision,
    pub style_json: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct RenderTaskView {
    pub id: TaskId,
    pub style: StyleRevision,
    pub source: Option<SourceRef>,
    pub request: RenderRequest,
    pub pixel_ratio: PixelRatio,
    pub output_format: ImageFormat,
    pub deadline: Instant,
}

impl From<&InternalTask> for RenderTaskView {
    fn from(task: &InternalTask) -> Self {
        Self {
            id: task.id,
            style: task.style.clone(),
            source: task.source.clone(),
            request: task.request.clone(),
            pixel_ratio: task.pixel_ratio,
            output_format: task.output_format,
            deadline: task.deadline,
        }
    }
}

/// Blocking renderer implementation owned by `RendererActor`'s OS thread.
///
/// Keeping the backend synchronous makes thread affinity explicit and prevents
/// accidental calls from tokio worker tasks.
pub trait BlockingRenderBackend: 'static {
    fn load_profile(
        &mut self,
        style: &ResolvedStyle,
        task: &RenderTaskView,
    ) -> Result<(), RendererError>;
    fn render(&mut self, task: &RenderTaskView) -> Result<RenderOutput, RendererError>;
    fn error_invalidates_loaded_state(&self, _err: &RendererError) -> bool {
        true
    }
    fn reset(&mut self) {}
}

pub struct RendererActor {
    tx: mpsc::Sender<RenderCmd>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

enum RenderCmd {
    LoadProfile {
        style: ResolvedStyle,
        task: RenderTaskView,
        reply: oneshot::Sender<Result<(), RendererError>>,
    },
    Render {
        task: RenderTaskView,
        reply: oneshot::Sender<Result<RenderOutput, RendererError>>,
    },
    Shutdown,
}

impl RendererActor {
    pub fn spawn(config: RendererActorConfig) -> Result<Self, RendererError> {
        let ambient_cache_path = config.ambient_cache_path.clone();
        Self::spawn_with_backend_factory(config, move || {
            MapLibreNativeBackend::new(ambient_cache_path)
        })
    }

    pub fn spawn_with_backend<B>(
        config: RendererActorConfig,
        backend: B,
    ) -> Result<Self, RendererError>
    where
        B: BlockingRenderBackend + Send,
    {
        Self::spawn_with_backend_factory(config, || backend)
    }

    fn spawn_with_backend_factory<F, B>(
        config: RendererActorConfig,
        backend_factory: F,
    ) -> Result<Self, RendererError>
    where
        F: FnOnce() -> B + Send + 'static,
        B: BlockingRenderBackend,
    {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(format!("biei-renderer-{}", config.worker_id))
            .spawn(move || run_actor(rx, backend_factory()))
            .map_err(|err| {
                RendererError::RenderFailed(format!("failed to spawn renderer actor: {err}"))
            })?;

        Ok(Self {
            tx,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn is_alive(&self) -> bool {
        let Ok(thread) = self.thread.lock() else {
            return false;
        };
        thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub async fn load_profile(
        &self,
        style: ResolvedStyle,
        task: RenderTaskView,
    ) -> Result<(), RendererError> {
        let (reply, rx) = oneshot::channel();
        let deadline = task.deadline;
        self.tx
            .send(RenderCmd::LoadProfile { style, task, reply })
            .map_err(|_| RendererError::ActorDead)?;
        await_actor_reply(deadline, rx).await
    }

    pub async fn render(&self, task: RenderTaskView) -> Result<RenderOutput, RendererError> {
        let (reply, rx) = oneshot::channel();
        let deadline = task.deadline;
        self.tx
            .send(RenderCmd::Render { task, reply })
            .map_err(|_| RendererError::ActorDead)?;
        await_actor_reply(deadline, rx).await
    }
}

impl Drop for RendererActor {
    fn drop(&mut self) {
        let _ = self.tx.send(RenderCmd::Shutdown);
        let Ok(mut thread) = self.thread.lock() else {
            return;
        };
        if let Some(thread) = thread.take() {
            let join_deadline = std::time::Instant::now() + ACTOR_JOIN_GRACE;
            while !thread.is_finished() && std::time::Instant::now() < join_deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!(
                    "renderer actor thread did not stop promptly; detaching to avoid blocking shutdown"
                );
            }
        }
    }
}

async fn await_actor_reply<T>(
    deadline: Instant,
    rx: oneshot::Receiver<Result<T, RendererError>>,
) -> Result<T, RendererError> {
    match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(RendererError::ActorDead),
        Err(_) => Err(RendererError::Timeout),
    }
}

fn run_actor<B>(rx: mpsc::Receiver<RenderCmd>, mut backend: B)
where
    B: BlockingRenderBackend,
{
    let mut loaded: Option<StyleRevision> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            RenderCmd::LoadProfile { style, task, reply } => {
                let revision = style.revision.clone();
                let result =
                    catch_backend_unwind("load_profile", || backend.load_profile(&style, &task));
                if result.is_ok() {
                    loaded = Some(revision);
                } else {
                    loaded = None;
                    reset_backend_after_error(&mut backend);
                }
                let _ = reply.send(result);
            }
            RenderCmd::Render { task, reply } => {
                let result = if loaded.as_ref() == Some(&task.style) {
                    catch_backend_unwind("render", || backend.render(&task))
                } else {
                    Err(RendererError::StyleNotReady {
                        style_id: task.style.id.clone(),
                        version: task.style.version,
                    })
                };
                let panicked = result
                    .as_ref()
                    .is_err_and(|err| renderer_error_is_actor_panic(err));
                if panicked
                    || result
                        .as_ref()
                        .is_err_and(|err| backend.error_invalidates_loaded_state(err))
                {
                    loaded = None;
                    reset_backend_after_error(&mut backend);
                }
                let _ = reply.send(result);
            }
            RenderCmd::Shutdown => break,
        }
    }
}

fn reset_backend_after_error<B>(backend: &mut B)
where
    B: BlockingRenderBackend,
{
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| backend.reset())) {
        let message = panic_payload_message(&payload);
        tracing::error!(
            panic = %message,
            "renderer actor backend reset panicked; keeping actor alive with cleared warm state"
        );
    }
}

fn catch_backend_unwind<T>(
    operation: &'static str,
    f: impl FnOnce() -> Result<T, RendererError>,
) -> Result<T, RendererError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = panic_payload_message(&payload);
            tracing::error!(
                operation,
                panic = %message,
                "renderer actor backend panicked; invalidating loaded state"
            );
            Err(RendererError::RenderFailed(format!(
                "renderer actor panicked during {operation}: {message}"
            )))
        }
    }
}

fn renderer_error_is_actor_panic(err: &RendererError) -> bool {
    matches!(err, RendererError::RenderFailed(message) if message.starts_with("renderer actor panicked during "))
}

fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

struct MapLibreNativeBackend {
    loaded_style: Option<ResolvedStyle>,
    active_renderer: Option<ActiveRenderer>,
    ambient_cache_path: Option<PathBuf>,
}

enum ActiveRenderer {
    Static {
        key: RendererKey,
        loaded_style: Option<StyleRevision>,
        observer_state: ObserverState,
        renderer: maplibre_native::ImageRenderer<maplibre_native::Static>,
        /// Pre-allocated overlay slots (style-setup-time fixed). Per-request
        /// overlay rendering only updates each slot's GeoJSON source via
        /// `source_mut(...).set_geojson(...)` and never adds/removes layers,
        /// so per-request expression-compile cost is paid once at style load.
        slots: OverlaySlotPool,
        /// Stable request-local addlayer sources kept on the loaded style.
        /// Request-local layers are removed after each render; unreferenced
        /// sources are harmless and let repeated tilesets avoid add_source.
        addlayer_sources: AddLayerSourceCache,
    },
    Tile {
        key: RendererKey,
        loaded_style: Option<StyleRevision>,
        observer_state: ObserverState,
        renderer: maplibre_native::ImageRenderer<maplibre_native::Tile>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RendererKey {
    render_mode: crate::types::RenderMode,
    pixel_ratio_bits: u32,
}

impl RendererKey {
    fn new(render_mode: crate::types::RenderMode, pixel_ratio: PixelRatio) -> Self {
        Self {
            render_mode,
            pixel_ratio_bits: pixel_ratio.as_f32().to_bits(),
        }
    }

    fn pixel_ratio(self) -> f32 {
        f32::from_bits(self.pixel_ratio_bits)
    }
}

impl MapLibreNativeBackend {
    fn new(ambient_cache_path: Option<PathBuf>) -> Self {
        Self {
            loaded_style: None,
            active_renderer: None,
            ambient_cache_path,
        }
    }

    fn style(&self) -> Result<&ResolvedStyle, RendererError> {
        self.loaded_style.as_ref().ok_or_else(|| {
            RendererError::RenderFailed("style has not been loaded in renderer backend".to_string())
        })
    }

    fn ensure_static_renderer(
        &mut self,
        key: RendererKey,
        size: maplibre_native::Size,
        deadline: Instant,
    ) -> Result<
        (
            &mut maplibre_native::ImageRenderer<maplibre_native::Static>,
            &mut OverlaySlotPool,
            &mut AddLayerSourceCache,
        ),
        RendererError,
    > {
        let style = self.style()?.clone();
        let needs_rebuild = !matches!(
            self.active_renderer,
            Some(ActiveRenderer::Static { key: existing, .. }) if existing == key
        );
        if needs_rebuild {
            let mut renderer = build_renderer(key, size, self.ambient_cache_path.as_deref())?
                .build_static_renderer();
            let observer_state = ObserverState::default();
            install_map_observer(&mut renderer, observer_state.clone());
            load_style_json(&mut renderer, &style, &observer_state, deadline)?;
            let slots = populate_static_slots(&mut renderer).map_err(|err| {
                RendererError::StyleLoadFailed {
                    style_id: style.revision.id.clone(),
                    source: err.to_string(),
                }
            })?;
            self.active_renderer = Some(ActiveRenderer::Static {
                key,
                loaded_style: Some(style.revision.clone()),
                observer_state,
                renderer,
                slots,
                addlayer_sources: AddLayerSourceCache::new(),
            });
        }
        let Some(ActiveRenderer::Static {
            loaded_style,
            observer_state,
            renderer,
            slots,
            addlayer_sources,
            ..
        }) = self.active_renderer.as_mut()
        else {
            unreachable!("static renderer was inserted")
        };
        if loaded_style.as_ref() != Some(&style.revision) {
            load_style_json(renderer, &style, observer_state, deadline)?;
            *loaded_style = Some(style.revision.clone());
            *slots =
                populate_static_slots(renderer).map_err(|err| RendererError::StyleLoadFailed {
                    style_id: style.revision.id.clone(),
                    source: err.to_string(),
                })?;
            *addlayer_sources = AddLayerSourceCache::new();
        }
        renderer.set_map_size(size);
        Ok((renderer, slots, addlayer_sources))
    }

    fn ensure_tile_renderer(
        &mut self,
        key: RendererKey,
        size: maplibre_native::Size,
        deadline: Instant,
    ) -> Result<&mut maplibre_native::ImageRenderer<maplibre_native::Tile>, RendererError> {
        let style = self.style()?.clone();
        let needs_rebuild = !matches!(
            self.active_renderer,
            Some(ActiveRenderer::Tile { key: existing, .. }) if existing == key
        );
        if needs_rebuild {
            let mut renderer = build_renderer(key, size, self.ambient_cache_path.as_deref())?
                .build_tile_renderer();
            let observer_state = ObserverState::default();
            install_map_observer(&mut renderer, observer_state.clone());
            load_style_json(&mut renderer, &style, &observer_state, deadline)?;
            self.active_renderer = Some(ActiveRenderer::Tile {
                key,
                loaded_style: Some(style.revision.clone()),
                observer_state,
                renderer,
            });
        }
        let Some(ActiveRenderer::Tile {
            loaded_style,
            observer_state,
            renderer,
            ..
        }) = self.active_renderer.as_mut()
        else {
            unreachable!("tile renderer was inserted")
        };
        if loaded_style.as_ref() != Some(&style.revision) {
            load_style_json(renderer, &style, observer_state, deadline)?;
            *loaded_style = Some(style.revision.clone());
        }
        renderer.set_map_size(size);
        Ok(renderer)
    }

    fn ensure_renderer_for_task(&mut self, task: &RenderTaskView) -> Result<(), RendererError> {
        match task.request {
            RenderRequest::Tile { tile_size, .. } => {
                let key = RendererKey::new(crate::types::RenderMode::Tile, task.pixel_ratio);
                let size = render_size(tile_size, tile_size)?;
                self.ensure_tile_renderer(key, size, task.deadline)?;
            }
            RenderRequest::StaticImage { width, height, .. } => {
                let key = RendererKey::new(crate::types::RenderMode::Static, task.pixel_ratio);
                let size = render_size(width, height)?;
                self.ensure_static_renderer(key, size, task.deadline)?;
            }
        }
        Ok(())
    }

    fn reset_loaded_state(&mut self) {
        self.loaded_style = None;
        match self.active_renderer.as_mut() {
            Some(ActiveRenderer::Static { loaded_style, .. })
            | Some(ActiveRenderer::Tile { loaded_style, .. }) => {
                *loaded_style = None;
            }
            None => {}
        }
    }
}

#[derive(Clone, Default)]
struct ObserverState {
    inner: Arc<Mutex<ObserverFlags>>,
}

#[derive(Default)]
struct ObserverFlags {
    style_loaded: bool,
    failure: Option<MapLoadFailure>,
}

#[derive(Clone, Debug)]
struct MapLoadFailure {
    error: maplibre_native::MapLoadError,
}

struct ObserverSnapshot {
    style_loaded: bool,
    failure: Option<MapLoadFailure>,
}

impl ObserverState {
    fn start_loading(&self) {
        let mut flags = self.inner.lock().expect("map observer state poisoned");
        flags.style_loaded = false;
        flags.failure = None;
    }

    fn finish_loading_style(&self) {
        let mut flags = self.inner.lock().expect("map observer state poisoned");
        flags.style_loaded = true;
    }

    fn fail_loading_map(&self, error: maplibre_native::MapLoadError) {
        let mut flags = self.inner.lock().expect("map observer state poisoned");
        flags.style_loaded = false;
        flags.failure = Some(MapLoadFailure { error });
    }

    fn snapshot(&self) -> ObserverSnapshot {
        let flags = self.inner.lock().expect("map observer state poisoned");
        ObserverSnapshot {
            style_loaded: flags.style_loaded,
            failure: flags.failure.clone(),
        }
    }
}

impl BlockingRenderBackend for MapLibreNativeBackend {
    fn load_profile(
        &mut self,
        style: &ResolvedStyle,
        task: &RenderTaskView,
    ) -> Result<(), RendererError> {
        self.loaded_style = Some(style.clone());
        self.ensure_renderer_for_task(task)
    }

    fn render(&mut self, task: &RenderTaskView) -> Result<RenderOutput, RendererError> {
        let image = match task.request {
            RenderRequest::Tile { z, x, y, tile_size } => {
                let key = RendererKey::new(crate::types::RenderMode::Tile, task.pixel_ratio);
                let size = render_size(tile_size, tile_size)?;
                self.ensure_tile_renderer(key, size, task.deadline)?
                    .render_tile(z, x, y)
            }
            RenderRequest::StaticImage {
                positioning:
                    crate::types::Positioning::Center {
                        lon,
                        lat,
                        zoom,
                        bearing,
                        pitch,
                    },
                width,
                height,
                ref overlays,
                ref before_layer,
                padding: _,
                ref addlayer,
            } => {
                let key = RendererKey::new(crate::types::RenderMode::Static, task.pixel_ratio);
                let size = render_size(width, height)?;
                let (renderer, slots, addlayer_sources) =
                    self.ensure_static_renderer(key, size, task.deadline)?;
                let camera = maplibre_native::CameraUpdate::new()
                    .center(maplibre_native::LatLng { lat, lng: lon })
                    .zoom(zoom)
                    .bearing(f64::from(bearing))
                    .pitch(f64::from(pitch));
                render_static_with_overlays_and_addlayer(
                    renderer,
                    slots,
                    addlayer_sources,
                    &camera,
                    overlays,
                    before_layer.as_deref(),
                    addlayer.as_ref(),
                    task.id,
                )
            }
            RenderRequest::StaticImage {
                positioning:
                    crate::types::Positioning::Bbox {
                        min_lon,
                        min_lat,
                        max_lon,
                        max_lat,
                    },
                width,
                height,
                ref overlays,
                ref before_layer,
                padding,
                ref addlayer,
            } => {
                let key = RendererKey::new(crate::types::RenderMode::Static, task.pixel_ratio);
                let size = render_size(width, height)?;
                let (renderer, slots, addlayer_sources) =
                    self.ensure_static_renderer(key, size, task.deadline)?;
                let bounds = maplibre_native::LatLngBounds {
                    southwest: maplibre_native::LatLng {
                        lat: min_lat,
                        lng: min_lon,
                    },
                    northeast: maplibre_native::LatLng {
                        lat: max_lat,
                        lng: max_lon,
                    },
                };
                let camera = renderer.camera_for_bounds(
                    bounds,
                    Some(padding_to_edge_insets(padding)),
                    0.0,
                    0.0,
                );
                render_static_with_overlays_and_addlayer(
                    renderer,
                    slots,
                    addlayer_sources,
                    &camera,
                    overlays,
                    before_layer.as_deref(),
                    addlayer.as_ref(),
                    task.id,
                )
            }
            RenderRequest::StaticImage {
                positioning: crate::types::Positioning::Auto,
                width,
                height,
                ref overlays,
                ref before_layer,
                padding,
                ref addlayer,
            } => {
                let key = RendererKey::new(crate::types::RenderMode::Static, task.pixel_ratio);
                let size = render_size(width, height)?;
                let (renderer, slots, addlayer_sources) =
                    self.ensure_static_renderer(key, size, task.deadline)?;
                // Build the overlay geometry collection once and ask mbgl
                // for a camera that fits it. The same overlays then get
                // installed by `assign_slots` below — that re-builds a
                // separate (idx-tagged) GeoJSON, which is a small cost we
                // accept for path simplicity.
                let fit_geojson = build_overlay_geojson(overlays)
                    .map_err(|err| RendererError::RenderFailed(err.to_string()))?;
                let auto_padding = padding_to_edge_insets(auto_padding_for_overlays(
                    padding, overlays, width, height,
                ));
                let Some(camera) =
                    renderer.camera_for_geojson(&fit_geojson, Some(auto_padding), 0.0, 0.0)
                else {
                    return Err(RendererError::RenderFailed(
                        "auto positioning: overlays produced no fittable geometry".to_string(),
                    ));
                };
                render_static_with_overlays_and_addlayer(
                    renderer,
                    slots,
                    addlayer_sources,
                    &camera,
                    overlays,
                    before_layer.as_deref(),
                    addlayer.as_ref(),
                    task.id,
                )
            }
        }
        .map_err(|err| RendererError::RenderFailed(err.to_string()))?;

        encode_image(&image, task.output_format)
    }

    fn error_invalidates_loaded_state(&self, err: &RendererError) -> bool {
        !matches!(err, RendererError::RenderFailed(_))
    }

    fn reset(&mut self) {
        self.reset_loaded_state();
    }
}

fn build_renderer(
    key: RendererKey,
    size: maplibre_native::Size,
    ambient_cache_path: Option<&std::path::Path>,
) -> Result<maplibre_native::ImageRendererBuilder, RendererError> {
    use std::num::NonZeroU32;

    let width = NonZeroU32::new(size.width)
        .ok_or_else(|| RendererError::RenderFailed("render width must be non-zero".to_string()))?;
    let height = NonZeroU32::new(size.height)
        .ok_or_else(|| RendererError::RenderFailed("render height must be non-zero".to_string()))?;

    let mut builder = maplibre_native::ImageRendererBuilder::new()
        .with_size(width, height)
        .with_pixel_ratio(key.pixel_ratio());
    if let Some(path) = ambient_cache_path {
        let resource_options =
            maplibre_native::ResourceOptions::default().with_cache_path(path.to_path_buf());
        builder = builder.with_resource_options(resource_options);
    }
    Ok(builder)
}

fn install_map_observer<S>(renderer: &mut maplibre_native::ImageRenderer<S>, state: ObserverState) {
    let observer = renderer.map_observer();
    observer.set_will_start_loading_map_callback({
        let state = state.clone();
        move || state.start_loading()
    });
    observer.set_did_finish_loading_style_callback({
        let state = state.clone();
        move || state.finish_loading_style()
    });
    observer.set_did_fail_loading_map_callback(move |error| {
        state.fail_loading_map(error);
    });
}

fn render_size(width: u16, height: u16) -> Result<maplibre_native::Size, RendererError> {
    if width == 0 || height == 0 {
        return Err(RendererError::RenderFailed(
            "render size must be non-zero".to_string(),
        ));
    }
    Ok(maplibre_native::Size {
        width: u32::from(width),
        height: u32::from(height),
    })
}

fn load_style_json<S>(
    renderer: &mut maplibre_native::ImageRenderer<S>,
    style: &ResolvedStyle,
    observer_state: &ObserverState,
    deadline: Instant,
) -> Result<(), RendererError> {
    observer_state.start_loading();
    // load_style_from_json_str returns a non-Send request handle; immediate
    // drop is fine. Completion is observed via did_finish_loading_style and
    // wait_for_style_load's tick loop.
    let _ = renderer.load_style_from_json_str(&style.style_json);
    wait_for_style_load(observer_state, style, deadline)
}

fn wait_for_style_load(
    state: &ObserverState,
    style: &ResolvedStyle,
    deadline: Instant,
) -> Result<(), RendererError> {
    let run_loop = maplibre_native::RunLoopHandle::current();
    loop {
        let snapshot = state.snapshot();
        if let Some(failure) = snapshot.failure {
            return Err(style_load_failure(&style.revision, failure));
        }
        if snapshot.style_loaded {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RendererError::Timeout);
        }
        run_loop.tick();
    }
}

fn style_load_failure(revision: &StyleRevision, failure: MapLoadFailure) -> RendererError {
    RendererError::StyleLoadFailed {
        style_id: revision.id.clone(),
        source: failure.error.to_string(),
    }
}

fn encode_image(
    image: &maplibre_native::Image,
    format: ImageFormat,
) -> Result<RenderOutput, RendererError> {
    use image::ImageEncoder;

    let buffer = image.as_image();
    let raw = buffer.as_raw();
    let width = buffer.width();
    let height = buffer.height();
    // 4 byte/pixel(RGBA8)。PNG/WebP の圧縮率は典型 30-60% 程度なので、
    // raw の半分程度を初期容量にすると再 alloc を概ね 1 回以内に抑えられる。
    let mut bytes: Vec<u8> = Vec::with_capacity(raw.len() / 2);

    match format {
        ImageFormat::Png => {
            // `flate2` の backend を `zlib-ng` に揃えてあるので(`biei/Cargo.toml`
            // 経由で workspace dep の features = ["zlib-ng"] が有効)、zlib level 6
            // (Default)でも encode 速度は旧 miniz_oxide Fast と同等。一方で
            // 出力サイズは Fast 比 ~25% 削減できる。大規模 CDN deploy では bytes
            // が支配的コストになるので、speed-vs-size のバランスを Default に倒す。
            // Filter は per-tile で内容が変わる map tile / static image だと
            // Sub 固定が無難(Adaptive は heuristic 計算が重い)。
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut bytes,
                image::codecs::png::CompressionType::Default,
                image::codecs::png::FilterType::Sub,
            );
            encoder
                .write_image(raw, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|err| RendererError::RenderFailed(format!("png encode failed: {err}")))?;
        }
        ImageFormat::Webp => {
            bytes = encode_webp_lossy(raw, width, height)?;
        }
        ImageFormat::Jpeg => {
            // JPEG has no alpha channel. Blend any non-opaque pixel onto a
            // white background before encoding; normal map renders are opaque.
            let rgb = rgba_to_rgb_on_white(raw);
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85);
            encoder
                .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|err| RendererError::RenderFailed(format!("jpg encode failed: {err}")))?;
        }
    }

    Ok(RenderOutput {
        bytes: bytes.into(),
        format,
    })
}

fn encode_webp_lossy(raw: &[u8], width: u32, height: u32) -> Result<Vec<u8>, RendererError> {
    const WEBP_QUALITY: f32 = 85.0;

    let encoder = webp::Encoder::from_rgba(raw, width, height);
    let encoded = encoder.encode(WEBP_QUALITY);
    Ok(encoded.to_vec())
}

fn rgba_to_rgb_on_white(raw: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(raw.len() / 4 * 3);
    for px in raw.chunks_exact(4) {
        let alpha = px[3] as u16;
        if alpha == 255 {
            rgb.extend_from_slice(&px[..3]);
        } else if alpha == 0 {
            rgb.extend_from_slice(&[255, 255, 255]);
        } else {
            for channel in &px[..3] {
                let blended = (*channel as u16 * alpha + 255 * (255 - alpha) + 127) / 255;
                rgb.push(blended as u8);
            }
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LngLat, PinOverlay, PinSize, Positioning, StyleId};

    struct FakeBackend;

    impl BlockingRenderBackend for FakeBackend {
        fn load_profile(
            &mut self,
            _style: &ResolvedStyle,
            _task: &RenderTaskView,
        ) -> Result<(), RendererError> {
            Ok(())
        }

        fn render(&mut self, task: &RenderTaskView) -> Result<RenderOutput, RendererError> {
            Ok(RenderOutput {
                bytes: vec![task.id as u8].into(),
                format: task.output_format,
            })
        }
    }

    struct SlowBackend;

    impl BlockingRenderBackend for SlowBackend {
        fn load_profile(
            &mut self,
            _style: &ResolvedStyle,
            _task: &RenderTaskView,
        ) -> Result<(), RendererError> {
            Ok(())
        }

        fn render(&mut self, task: &RenderTaskView) -> Result<RenderOutput, RendererError> {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(RenderOutput {
                bytes: vec![task.id as u8].into(),
                format: task.output_format,
            })
        }
    }

    struct ResetCountingBackend {
        resets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl BlockingRenderBackend for ResetCountingBackend {
        fn load_profile(
            &mut self,
            _style: &ResolvedStyle,
            _task: &RenderTaskView,
        ) -> Result<(), RendererError> {
            Ok(())
        }

        fn render(&mut self, _task: &RenderTaskView) -> Result<RenderOutput, RendererError> {
            Err(RendererError::RenderFailed(
                "test render failure".to_string(),
            ))
        }

        fn reset(&mut self) {
            self.resets
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    struct PanickingBackend {
        resets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl BlockingRenderBackend for PanickingBackend {
        fn load_profile(
            &mut self,
            _style: &ResolvedStyle,
            _task: &RenderTaskView,
        ) -> Result<(), RendererError> {
            Ok(())
        }

        fn render(&mut self, _task: &RenderTaskView) -> Result<RenderOutput, RendererError> {
            panic!("synthetic renderer panic");
        }

        fn reset(&mut self) {
            self.resets
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    fn revision() -> StyleRevision {
        StyleRevision {
            id: StyleId("carto/voyager".to_string()),
            version: 1,
        }
    }

    fn resolved_style() -> ResolvedStyle {
        ResolvedStyle {
            revision: revision(),
            style_json: Arc::from(r#"{"version":8,"sources":{},"layers":[]}"#),
        }
    }

    fn pin(size: PinSize) -> StaticOverlay {
        StaticOverlay::Pin(PinOverlay {
            size,
            label: None,
            color: "4c78a8".to_string(),
            coordinate: LngLat {
                lon: 139.767,
                lat: 35.681,
            },
        })
    }

    #[test]
    fn stable_addlayer_source_id_depends_on_tileset_and_json() {
        let source = crate::types::AddLayerSource {
            tileset_id: "rain".to_string(),
            json: r#"{"type":"vector","tiles":["https://example.test/{z}/{x}/{y}.pbf"]}"#
                .to_string(),
        };
        let same = crate::types::AddLayerSource {
            tileset_id: "rain".to_string(),
            json: source.json.clone(),
        };
        let different_tileset = crate::types::AddLayerSource {
            tileset_id: "snow".to_string(),
            json: source.json.clone(),
        };
        let different_json = crate::types::AddLayerSource {
            tileset_id: "rain".to_string(),
            json: r#"{"type":"vector","tiles":["https://other.example.test/{z}/{x}/{y}.pbf"]}"#
                .to_string(),
        };

        assert_eq!(source.stable_source_id(), same.stable_source_id());
        assert_ne!(
            source.stable_source_id(),
            different_tileset.stable_source_id()
        );
        assert_ne!(source.stable_source_id(), different_json.stable_source_id());
    }

    #[test]
    fn rgba_to_rgb_on_white_blends_alpha_for_jpeg() {
        let rgba = [
            10, 20, 30, 255, //
            10, 20, 30, 0, //
            0, 0, 0, 128,
        ];

        assert_eq!(
            rgba_to_rgb_on_white(&rgba),
            vec![
                10, 20, 30, //
                255, 255, 255, //
                127, 127, 127,
            ]
        );
    }

    fn task_view(style: StyleRevision) -> RenderTaskView {
        RenderTaskView {
            id: 7,
            style,
            source: None,
            request: RenderRequest::StaticImage {
                positioning: Positioning::Center {
                    lon: 139.767,
                    lat: 35.681,
                    zoom: 12.0,
                    bearing: 0.0,
                    pitch: 0.0,
                },
                width: 512,
                height: 512,
                overlays: Vec::new(),
                before_layer: None,
                padding: Padding::default(),
                addlayer: None,
            },
            pixel_ratio: PixelRatio::X1,
            output_format: ImageFormat::Png,
            deadline: Instant::now() + std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn auto_padding_adds_pin_top_inset() {
        let base = Padding {
            top: 20,
            right: 26,
            bottom: 20,
            left: 26,
        };
        let overlays = vec![
            StaticOverlay::Path(crate::types::PathOverlay {
                stroke_width: None,
                stroke_color: None,
                stroke_opacity: None,
                fill_color: None,
                fill_opacity: None,
                coordinates: vec![
                    LngLat {
                        lon: 139.767,
                        lat: 35.0,
                    },
                    LngLat {
                        lon: 139.767,
                        lat: 35.681,
                    },
                ],
            }),
            pin(PinSize::Large),
        ];

        assert_eq!(
            auto_padding_for_overlays(base, &overlays, 300, 190),
            Padding {
                top: 46,
                right: 26,
                bottom: 20,
                left: 26,
            }
        );
    }

    #[test]
    fn auto_padding_ignores_non_pin_overlays() {
        let base = Padding::all(10);
        assert_eq!(auto_padding_for_overlays(base, &[], 300, 190), base);
    }

    #[test]
    fn auto_padding_only_counts_pins_on_bounds_edges() {
        let base = Padding::all(10);
        let overlays = vec![
            pin(PinSize::Small),
            StaticOverlay::Path(crate::types::PathOverlay {
                stroke_width: None,
                stroke_color: None,
                stroke_opacity: None,
                fill_color: None,
                fill_opacity: None,
                coordinates: vec![
                    LngLat {
                        lon: 138.0,
                        lat: 36.0,
                    },
                    LngLat {
                        lon: 140.0,
                        lat: 36.0,
                    },
                ],
            }),
        ];

        assert_eq!(
            auto_padding_for_overlays(base, &overlays, 300, 190),
            Padding {
                top: 10,
                right: 10,
                bottom: 10,
                left: 10,
            }
        );
    }

    #[test]
    fn auto_padding_counts_pins_near_bounds_edges() {
        let base = Padding::all(10);
        let overlays = vec![
            StaticOverlay::GeoJson(crate::types::GeoJsonOverlay {
                feature_collection: serde_json::json!({
                    "type": "Feature",
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [[
                            [-122.4111, 37.770025],
                            [-122.372037, 37.738775],
                            [-122.309537, 37.762213],
                            [-122.270475, 37.801275],
                            [-122.293912, 37.863775],
                            [-122.340787, 37.895025],
                            [-122.395475, 37.84815],
                            [-122.4111, 37.770025]
                        ]]
                    },
                    "properties": {}
                }),
            }),
            StaticOverlay::Pin(PinOverlay {
                size: PinSize::Small,
                label: None,
                color: "4682b4".to_string(),
                coordinate: LngLat {
                    lon: -122.4486,
                    lat: 37.8269,
                },
            }),
            StaticOverlay::Pin(PinOverlay {
                size: PinSize::Small,
                label: None,
                color: "4682b4".to_string(),
                coordinate: LngLat {
                    lon: -122.54,
                    lat: 36.7761,
                },
            }),
        ];

        let padding = auto_padding_for_overlays(base, &overlays, 300, 190);
        assert!(
            padding.top > base.top,
            "pin near the north edge needs extra top padding"
        );
        assert_eq!(padding.right, base.right);
        assert_eq!(padding.left, base.left);
    }

    #[tokio::test]
    async fn actor_loads_style_and_renders_on_backend_thread() {
        let actor = RendererActor::spawn_with_backend(
            RendererActorConfig {
                worker_id: 3,
                ambient_cache_path: None,
            },
            FakeBackend,
        )
        .expect("actor spawns");
        let style = resolved_style();
        let rev = style.revision.clone();

        let task = task_view(rev);
        actor
            .load_profile(style, task.clone())
            .await
            .expect("profile loads");
        let output = actor.render(task).await.expect("render succeeds");

        assert_eq!(output.bytes.as_ref(), &[7]);
        assert_eq!(output.format, ImageFormat::Png);
        assert!(actor.is_alive());
    }

    #[tokio::test]
    async fn actor_rejects_render_before_matching_style_is_loaded() {
        let actor = RendererActor::spawn_with_backend(
            RendererActorConfig {
                worker_id: 4,
                ambient_cache_path: None,
            },
            FakeBackend,
        )
        .expect("actor spawns");

        let err = actor
            .render(task_view(revision()))
            .await
            .expect_err("style must be loaded first");

        assert!(matches!(err, RendererError::StyleNotReady { .. }));
    }

    #[tokio::test]
    async fn actor_reply_wait_respects_task_deadline() {
        let actor = RendererActor::spawn_with_backend(
            RendererActorConfig {
                worker_id: 5,
                ambient_cache_path: None,
            },
            SlowBackend,
        )
        .expect("actor spawns");
        let style = resolved_style();
        let rev = style.revision.clone();
        let mut task = task_view(rev);
        actor
            .load_profile(style, task.clone())
            .await
            .expect("profile loads");
        task.deadline = Instant::now() + std::time::Duration::from_millis(5);

        let err = actor
            .render(task)
            .await
            .expect_err("actor reply wait times out at task deadline");

        assert!(matches!(err, RendererError::Timeout));
    }

    #[tokio::test]
    async fn actor_resets_loaded_state_after_render_failure() {
        let resets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let actor = RendererActor::spawn_with_backend(
            RendererActorConfig {
                worker_id: 6,
                ambient_cache_path: None,
            },
            ResetCountingBackend {
                resets: resets.clone(),
            },
        )
        .expect("actor spawns");
        let style = resolved_style();
        let rev = style.revision.clone();
        let task = task_view(rev);
        actor
            .load_profile(style, task.clone())
            .await
            .expect("profile loads");

        let err = actor
            .render(task.clone())
            .await
            .expect_err("render failure is returned");
        assert!(matches!(err, RendererError::RenderFailed(_)));
        assert_eq!(resets.load(std::sync::atomic::Ordering::Acquire), 1);

        let err = actor
            .render(task)
            .await
            .expect_err("failed render clears actor warm state");
        assert!(matches!(err, RendererError::StyleNotReady { .. }));
    }

    #[tokio::test]
    async fn actor_survives_backend_panic_and_clears_loaded_state() {
        let resets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let actor = RendererActor::spawn_with_backend(
            RendererActorConfig {
                worker_id: 7,
                ambient_cache_path: None,
            },
            PanickingBackend {
                resets: resets.clone(),
            },
        )
        .expect("actor spawns");
        let style = resolved_style();
        let rev = style.revision.clone();
        let task = task_view(rev);
        actor
            .load_profile(style, task.clone())
            .await
            .expect("profile loads");

        let err = actor
            .render(task.clone())
            .await
            .expect_err("panic is mapped");
        assert!(matches!(
            err,
            RendererError::RenderFailed(message)
                if message.contains("renderer actor panicked during render")
        ));
        assert_eq!(resets.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(actor.is_alive());

        let err = actor
            .render(task)
            .await
            .expect_err("panic clears actor warm state");
        assert!(matches!(err, RendererError::StyleNotReady { .. }));
    }
}
