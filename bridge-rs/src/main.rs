use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info};
use zbus::{interface, Connection, SignalContext};

const DEFAULT_BUS_NAME: &str = "org.imalison.ChromeWindowInfo";
const DEFAULT_OBJECT_PATH: &str = "/org/imalison/ChromeWindowInfo";
const DEFAULT_SCHEMA: &str = "org.imalison.chrome_window_info.v1";

#[derive(Parser, Debug, Clone)]
#[command(about = "Chrome metadata to D-Bus bridge")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 38933)]
    port: u16,

    #[arg(long, default_value = "/update")]
    path: String,

    #[arg(long, default_value = DEFAULT_BUS_NAME)]
    bus_name: String,

    #[arg(long, default_value = DEFAULT_OBJECT_PATH)]
    object_path: String,

    #[arg(long)]
    token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<Value>,
    token: Option<String>,
}

#[derive(Clone)]
struct SharedDbusState {
    schema: String,
    last_payload: Arc<RwLock<String>>,
    window_payloads: Arc<RwLock<HashMap<String, Value>>>,
}

struct ChromeWindowInfoInterface {
    shared: SharedDbusState,
}

#[interface(name = "org.imalison.ChromeWindowInfo")]
impl ChromeWindowInfoInterface {
    async fn get_last_payload(&self) -> String {
        self.shared.last_payload.read().await.clone()
    }

    async fn get_window_payloads(&self) -> String {
        serde_json::to_string(&*self.shared.window_payloads.read().await)
            .unwrap_or_else(|_| "{}".to_owned())
    }

    async fn get_schema(&self) -> String {
        self.shared.schema.clone()
    }

    #[zbus(signal)]
    async fn updated(ctxt: &SignalContext<'_>, payload: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn window_updated(
        ctxt: &SignalContext<'_>,
        window_id: &str,
        payload: &str,
    ) -> zbus::Result<()>;
}

type SharedHyprlandState = Arc<RwLock<HyprlandState>>;

#[derive(Clone, Debug, Default, PartialEq)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct HyprClient {
    address: String,
    class_name: String,
    title: String,
    pid: Option<i64>,
    workspace_id: Option<i64>,
    focus_history_id: Option<i64>,
    rect: Option<Rect>,
}

#[derive(Clone, Debug, Default)]
struct HyprlandState {
    active_address: Option<String>,
    clients: HashMap<String, HyprClient>,
    event_stream_connected: bool,
    last_event: Option<String>,
    last_event_at: Option<String>,
    last_checked_at: Option<String>,
}

#[derive(Clone, Debug)]
struct WindowMapping {
    address: String,
    confidence: f64,
    sources: Vec<String>,
}

#[derive(Clone, Debug)]
struct CandidateScore {
    client: HyprClient,
    confidence: f64,
    sources: Vec<String>,
}

#[derive(Default)]
struct PayloadEnricher {
    window_map: HashMap<String, WindowMapping>,
    hyprland: SharedHyprlandState,
}

impl PayloadEnricher {
    async fn enrich(&mut self, mut payload: Value) -> Value {
        let tab_title = payload
            .pointer("/tab/title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let chrome_window = payload.pointer("/chrome_window").cloned();
        let chrome_window_id = chrome_window
            .as_ref()
            .and_then(|cw| cw.get("id"))
            .and_then(value_to_string);

        let chrome_window_focused = chrome_window
            .as_ref()
            .and_then(|cw| cw.get("focused"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        refresh_hyprland_state(&self.hyprland).await;
        let hyprland = self.hyprland.read().await.clone();

        prune_stale_window_mappings(&mut self.window_map, &hyprland);

        let chrome_rect = chrome_window.as_ref().and_then(rect_from_chrome_window);
        let candidate_scores = score_hyprland_candidates(
            &tab_title,
            chrome_window_focused,
            chrome_rect.as_ref(),
            chrome_window_id.as_deref(),
            &hyprland,
            &self.window_map,
        );
        let mapped_window = update_mapping_from_candidates(
            chrome_window_id.as_deref(),
            &candidate_scores,
            &mut self.window_map,
        )
        .or_else(|| cached_mapping_json(chrome_window_id.as_deref(), &self.window_map));

        let wm = build_wm_context(&tab_title, &hyprland, &candidate_scores);

        if !payload.is_object() {
            payload = json!({ "raw": payload });
        }

        if let Some(root) = payload.as_object_mut() {
            root.insert("wm".to_owned(), Value::Object(wm));
            root.insert(
                "bridge".to_owned(),
                json!({
                    "received_at": utc_now_iso(),
                    "schema": DEFAULT_SCHEMA,
                    "mapped_window": mapped_window,
                    "known_chrome_window_mappings": self.window_map.len()
                }),
            );
        }

        payload
    }
}

async fn handle_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Some(expected) = &state.token {
        let provided = headers
            .get("x-bridge-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        if provided != expected {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "invalid token" })),
            );
        }
    }

    if !payload.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "payload must be a JSON object" })),
        );
    }

    match state.tx.send(payload).await {
        Ok(_) => (StatusCode::ACCEPTED, Json(json!({ "ok": true }))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "bridge unavailable" })),
        ),
    }
}

async fn process_payloads(
    mut rx: mpsc::Receiver<Value>,
    shared: SharedDbusState,
    connection: Connection,
    object_path: String,
    hyprland: SharedHyprlandState,
) {
    let mut enricher = PayloadEnricher {
        window_map: HashMap::new(),
        hyprland,
    };

    while let Some(payload) = rx.recv().await {
        let enriched = enricher.enrich(payload).await;
        let payload_text = match serde_json::to_string(&enriched) {
            Ok(v) => v,
            Err(err) => {
                error!("failed to encode payload: {err}");
                continue;
            }
        };

        {
            let mut guard = shared.last_payload.write().await;
            *guard = payload_text.clone();
        }

        let mapped_window_id = enriched
            .pointer("/bridge/mapped_window/window_id")
            .and_then(Value::as_str)
            .map(str::to_owned);

        if let Some(window_id) = mapped_window_id.as_deref() {
            let mut guard = shared.window_payloads.write().await;
            guard.insert(window_id.to_owned(), enriched.clone());
        }

        let ctxt = match SignalContext::new(&connection, object_path.as_str()) {
            Ok(v) => v,
            Err(err) => {
                error!("failed to build dbus signal context: {err}");
                continue;
            }
        };

        if let Some(window_id) = mapped_window_id.as_deref() {
            if let Err(err) =
                ChromeWindowInfoInterface::window_updated(&ctxt, window_id, &payload_text).await
            {
                error!("failed to emit WindowUpdated signal: {err}");
            }
        }

        if let Err(err) = ChromeWindowInfoInterface::updated(&ctxt, &payload_text).await {
            error!("failed to emit Updated signal: {err}");
        }
    }
}

fn build_wm_context(
    tab_title: &str,
    hyprland: &HyprlandState,
    candidate_scores: &[CandidateScore],
) -> Map<String, Value> {
    let mut context = Map::new();
    context.insert("checked_at".to_owned(), Value::String(utc_now_iso()));

    let mut available_backends = Vec::new();

    if command_exists("hyprctl") || !hyprland.clients.is_empty() {
        available_backends.push(Value::String("hyprland".to_owned()));

        context.insert(
            "hyprland_event_stream_connected".to_owned(),
            Value::Bool(hyprland.event_stream_connected),
        );
        insert_optional_string(
            &mut context,
            "hyprland_last_event",
            hyprland.last_event.as_deref(),
        );
        insert_optional_string(
            &mut context,
            "hyprland_last_event_at",
            hyprland.last_event_at.as_deref(),
        );
        insert_optional_string(
            &mut context,
            "hyprland_last_checked_at",
            hyprland.last_checked_at.as_deref(),
        );

        if let Some(active_address) = &hyprland.active_address {
            if let Some(active) = hyprland.clients.get(active_address) {
                context.insert("hyprland_active".to_owned(), compact_hypr_client(active));
            } else {
                context.insert(
                    "hyprland_active".to_owned(),
                    json!({ "address": active_address }),
                );
            }
        }

        if !tab_title.is_empty() {
            let title_matches = hyprland
                .clients
                .values()
                .filter(|client| {
                    looks_like_chrome(&client.class_name)
                        && title_similarity(tab_title, &client.title) >= 0.5
                })
                .take(5)
                .map(compact_hypr_client)
                .collect::<Vec<_>>();
            context.insert(
                "hyprland_title_matches".to_owned(),
                Value::Array(title_matches),
            );
        }

        let candidates = candidate_scores
            .iter()
            .take(5)
            .map(candidate_score_json)
            .collect::<Vec<_>>();
        context.insert("hyprland_candidates".to_owned(), Value::Array(candidates));
    }

    if command_exists("xdotool") {
        available_backends.push(Value::String("x11".to_owned()));

        if let Some(active_id) = run_text_command("xdotool", &["getactivewindow"]) {
            let mut x11_active = Map::new();
            x11_active.insert("id_decimal".to_owned(), Value::String(active_id.clone()));

            let id_hex = active_id
                .parse::<u64>()
                .ok()
                .map(|window_id| format!("{window_id:#x}"))
                .unwrap_or_default();
            if !id_hex.is_empty() {
                x11_active.insert("id_hex".to_owned(), Value::String(id_hex));
            }

            if let Some(title) = run_text_command("xdotool", &["getwindowname", &active_id]) {
                x11_active.insert("title".to_owned(), Value::String(title));
            }

            context.insert("x11_active".to_owned(), Value::Object(x11_active));
        }
    }

    context.insert(
        "available_backends".to_owned(),
        Value::Array(available_backends),
    );

    context
}

fn compact_hypr_client(client: &HyprClient) -> Value {
    let mut obj = Map::new();

    obj.insert("address".to_owned(), Value::String(client.address.clone()));
    obj.insert("class".to_owned(), Value::String(client.class_name.clone()));
    obj.insert("title".to_owned(), Value::String(client.title.clone()));
    insert_optional_i64(&mut obj, "pid", client.pid);
    insert_optional_i64(&mut obj, "workspace_id", client.workspace_id);
    insert_optional_i64(&mut obj, "focus_history_id", client.focus_history_id);

    if let Some(rect) = &client.rect {
        obj.insert("bounds".to_owned(), rect_json(rect));
    }

    Value::Object(obj)
}

async fn refresh_hyprland_state(shared: &SharedHyprlandState) {
    if !command_exists("hyprctl") {
        return;
    }

    let checked_at = utc_now_iso();
    let active =
        run_hyprctl_json(&["-j", "activewindow"]).and_then(|value| hypr_client_from_value(&value));
    let clients = run_hyprctl_json(&["-j", "clients"]).and_then(|value| {
        value.as_array().map(|clients| {
            clients
                .iter()
                .filter_map(hypr_client_from_value)
                .map(|client| (client.address.clone(), client))
                .collect::<HashMap<_, _>>()
        })
    });

    let mut guard = shared.write().await;
    guard.last_checked_at = Some(checked_at);

    if let Some(clients) = clients {
        guard.clients = clients;
    }

    if let Some(active_client) = active {
        guard.active_address = Some(active_client.address.clone());
        guard
            .clients
            .insert(active_client.address.clone(), active_client);
    } else if guard
        .active_address
        .as_ref()
        .is_some_and(|address| !guard.clients.contains_key(address))
    {
        guard.active_address = None;
    }
}

async fn watch_hyprland_events(shared: SharedHyprlandState) {
    loop {
        let Some(path) = hyprland_socket2_path() else {
            sleep(Duration::from_secs(2)).await;
            continue;
        };

        match UnixStream::connect(&path).await {
            Ok(stream) => {
                {
                    let mut guard = shared.write().await;
                    guard.event_stream_connected = true;
                }

                let mut lines = BufReader::new(stream).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            apply_hyprland_event(&shared, &line).await;
                        }
                        Ok(None) => break,
                        Err(err) => {
                            debug!("hyprland IPC stream read failed: {err}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                debug!(
                    "failed to connect to Hyprland IPC socket {}: {err}",
                    path.display()
                );
            }
        }

        {
            let mut guard = shared.write().await;
            guard.event_stream_connected = false;
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn apply_hyprland_event(shared: &SharedHyprlandState, line: &str) {
    let mut parts = line.splitn(2, ">>");
    let event_name = parts.next().unwrap_or_default();
    let event_payload = parts.next().unwrap_or_default();
    let now = utc_now_iso();

    let mut guard = shared.write().await;
    guard.last_event = Some(line.to_owned());
    guard.last_event_at = Some(now);

    match event_name {
        "activewindowv2" => {
            if !event_payload.is_empty() {
                guard.active_address = Some(normalize_hypr_address(event_payload));
            }
        }
        "windowtitlev2" => {
            let mut values = event_payload.splitn(2, ',');
            if let (Some(address), Some(title)) = (values.next(), values.next()) {
                let address = normalize_hypr_address(address);
                if let Some(client) = guard.clients.get_mut(&address) {
                    client.title = title.to_owned();
                }
            }
        }
        "openwindow" => {
            let values = event_payload.splitn(4, ',').collect::<Vec<_>>();
            if values.len() == 4 {
                let address = normalize_hypr_address(values[0]);
                guard.clients.entry(address.clone()).or_insert(HyprClient {
                    address,
                    workspace_id: values[1].parse::<i64>().ok(),
                    class_name: values[2].to_owned(),
                    title: values[3].to_owned(),
                    ..HyprClient::default()
                });
            }
        }
        "closewindow" => {
            let address = normalize_hypr_address(event_payload);
            guard.clients.remove(&address);
            if guard.active_address.as_deref() == Some(address.as_str()) {
                guard.active_address = None;
            }
        }
        _ => {}
    }
}

fn hyprland_socket2_path() -> Option<PathBuf> {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")?;
    let signature = hyprland_instance_signature()?;
    Some(
        PathBuf::from(runtime_dir)
            .join("hypr")
            .join(signature)
            .join(".socket2.sock"),
    )
}

fn hyprland_instance_signature() -> Option<String> {
    env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .filter(|signature| !signature.is_empty())
        .or_else(|| {
            let runtime_dir = env::var_os("XDG_RUNTIME_DIR")?;
            let hypr_dir = PathBuf::from(runtime_dir).join("hypr");
            fs::read_dir(hypr_dir)
                .ok()?
                .filter_map(Result::ok)
                .find_map(|entry| {
                    if path_is_socket(entry.path().join(".socket.sock"))
                        || path_is_socket(entry.path().join(".socket2.sock"))
                    {
                        entry.file_name().into_string().ok()
                    } else {
                        None
                    }
                })
        })
}

fn path_is_socket(path: PathBuf) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

fn hypr_client_from_value(value: &Value) -> Option<HyprClient> {
    let address = normalize_hypr_address(value.get("address").and_then(Value::as_str)?);
    if address.is_empty() {
        return None;
    }

    let workspace_id = value
        .get("workspace")
        .and_then(Value::as_object)
        .and_then(|workspace| workspace.get("id"))
        .and_then(Value::as_i64);

    Some(HyprClient {
        address,
        class_name: value
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        pid: value.get("pid").and_then(Value::as_i64),
        workspace_id,
        focus_history_id: value.get("focusHistoryID").and_then(Value::as_i64),
        rect: rect_from_hypr_client(value),
    })
}

fn normalize_hypr_address(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("0x") {
        trimmed.to_owned()
    } else {
        format!("0x{trimmed}")
    }
}

fn rect_from_hypr_client(value: &Value) -> Option<Rect> {
    let at = value.get("at").and_then(Value::as_array)?;
    let size = value.get("size").and_then(Value::as_array)?;
    Some(Rect {
        x: at.first()?.as_i64()?,
        y: at.get(1)?.as_i64()?,
        width: size.first()?.as_i64()?,
        height: size.get(1)?.as_i64()?,
    })
}

fn rect_from_chrome_window(value: &Value) -> Option<Rect> {
    Some(Rect {
        x: value.get("left")?.as_i64()?,
        y: value.get("top")?.as_i64()?,
        width: value.get("width")?.as_i64()?,
        height: value.get("height")?.as_i64()?,
    })
}

fn score_hyprland_candidates(
    tab_title: &str,
    chrome_window_focused: bool,
    chrome_rect: Option<&Rect>,
    chrome_window_id: Option<&str>,
    hyprland: &HyprlandState,
    window_map: &HashMap<String, WindowMapping>,
) -> Vec<CandidateScore> {
    let cached_address = chrome_window_id
        .and_then(|id| window_map.get(id))
        .map(|mapping| mapping.address.as_str());

    let mut scores = hyprland
        .clients
        .values()
        .filter(|client| looks_like_chrome(&client.class_name))
        .map(|client| {
            let mut confidence: f64 = 0.05;
            let mut sources = vec!["chrome_class".to_owned()];

            if chrome_window_focused
                && hyprland.active_address.as_deref() == Some(client.address.as_str())
            {
                confidence += 0.55;
                sources.push("hyprland_active_window".to_owned());
            }

            let title_score = title_similarity(tab_title, &client.title);
            if title_score >= 0.9 {
                confidence += 0.55;
                sources.push("exact_or_contained_title".to_owned());
            } else if title_score >= 0.5 {
                confidence += 0.25;
                sources.push("partial_title".to_owned());
            }

            if let (Some(chrome), Some(hypr)) = (chrome_rect, client.rect.as_ref()) {
                let geometry_score = rect_similarity(chrome, hypr);
                if geometry_score >= 0.9 {
                    confidence += 0.3;
                    sources.push("geometry_match".to_owned());
                } else if geometry_score >= 0.7 {
                    confidence += 0.18;
                    sources.push("approximate_geometry".to_owned());
                }
            }

            if cached_address == Some(client.address.as_str()) {
                confidence += 0.2;
                sources.push("cached_mapping_continuity".to_owned());
            }

            CandidateScore {
                client: client.clone(),
                confidence: confidence.min(1.0),
                sources,
            }
        })
        .collect::<Vec<_>>();

    scores.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scores
}

fn update_mapping_from_candidates(
    chrome_window_id: Option<&str>,
    candidate_scores: &[CandidateScore],
    window_map: &mut HashMap<String, WindowMapping>,
) -> Option<Value> {
    let chrome_window_id = chrome_window_id?;
    let best = candidate_scores.first()?;
    let threshold = if window_map
        .get(chrome_window_id)
        .is_some_and(|mapping| mapping.address == best.client.address)
    {
        0.25
    } else {
        0.45
    };

    if best.confidence < threshold {
        return None;
    }

    let mapping = WindowMapping {
        address: best.client.address.clone(),
        confidence: best.confidence,
        sources: best.sources.clone(),
    };
    window_map.insert(chrome_window_id.to_owned(), mapping.clone());
    Some(mapping_json(&mapping))
}

fn cached_mapping_json(
    chrome_window_id: Option<&str>,
    window_map: &HashMap<String, WindowMapping>,
) -> Option<Value> {
    chrome_window_id
        .and_then(|id| window_map.get(id))
        .map(mapping_json)
}

fn prune_stale_window_mappings(
    window_map: &mut HashMap<String, WindowMapping>,
    hyprland: &HyprlandState,
) {
    window_map.retain(|_, mapping| hyprland.clients.contains_key(&mapping.address));
}

fn mapping_json(mapping: &WindowMapping) -> Value {
    json!({
        "backend": "hyprland",
        "window_id": mapping.address,
        "confidence": mapping.confidence,
        "sources": mapping.sources
    })
}

fn candidate_score_json(score: &CandidateScore) -> Value {
    let mut obj = compact_hypr_client(&score.client);
    if let Some(root) = obj.as_object_mut() {
        root.insert("confidence".to_owned(), json!(score.confidence));
        root.insert(
            "sources".to_owned(),
            Value::Array(
                score
                    .sources
                    .iter()
                    .map(|source| Value::String(source.clone()))
                    .collect(),
            ),
        );
    }
    obj
}

fn title_similarity(left: &str, right: &str) -> f64 {
    let left = left.trim().to_lowercase();
    let right = right.trim().to_lowercase();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    if left == right {
        return 1.0;
    }

    if left.contains(&right) || right.contains(&left) {
        return 0.92;
    }

    let left_words = significant_words(&left);
    let right_words = significant_words(&right);
    if left_words.is_empty() || right_words.is_empty() {
        return 0.0;
    }

    let overlap = left_words
        .iter()
        .filter(|word| right_words.contains(*word))
        .count();
    overlap as f64 / left_words.len().max(right_words.len()) as f64
}

fn significant_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() >= 3)
        .map(ToOwned::to_owned)
        .collect()
}

fn rect_similarity(left: &Rect, right: &Rect) -> f64 {
    let position_delta = (left.x - right.x).abs() + (left.y - right.y).abs();
    let size_delta = (left.width - right.width).abs() + (left.height - right.height).abs();

    if position_delta <= 8 && size_delta <= 16 {
        1.0
    } else if position_delta <= 32 && size_delta <= 64 {
        0.8
    } else if position_delta <= 96 && size_delta <= 160 {
        0.6
    } else {
        0.0
    }
}

fn rect_json(rect: &Rect) -> Value {
    json!({
        "x": rect.x,
        "y": rect.y,
        "width": rect.width,
        "height": rect.height
    })
}

fn insert_optional_string(target: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        target.insert(key.to_owned(), Value::String(v.to_owned()));
    }
}

fn insert_optional_i64(target: &mut Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(v) = value {
        target.insert(key.to_owned(), Value::Number(v.into()));
    }
}

fn command_exists(command: &str) -> bool {
    which::which(command).is_ok()
}

fn run_text_command(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn run_hyprctl_json(args: &[&str]) -> Option<Value> {
    let mut command = Command::new("hyprctl");
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Some(signature) = hyprland_instance_signature() {
            command.arg("-i").arg(signature);
        }
    }
    let output = command.args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    serde_json::from_str::<Value>(text.trim()).ok()
}

fn looks_like_chrome(class_name: &str) -> bool {
    let lowered = class_name.to_lowercase();
    ["chrome", "chromium", "brave", "edge", "vivaldi"]
        .iter()
        .any(|marker| lowered.contains(marker))
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(v) => Some(v.clone()),
        Value::Number(v) => Some(v.to_string()),
        Value::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

fn utc_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = ts.as_secs() as i64;
    let nanos = ts.subsec_nanos();

    let datetime = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
        .unwrap_or_else(chrono::Utc::now);
    datetime
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let (tx, rx) = mpsc::channel::<Value>(128);
    let app_state = AppState {
        tx,
        token: args.token.clone(),
    };

    let shared = SharedDbusState {
        schema: DEFAULT_SCHEMA.to_owned(),
        last_payload: Arc::new(RwLock::new("{}".to_owned())),
        window_payloads: Arc::new(RwLock::new(HashMap::new())),
    };
    let hyprland = Arc::new(RwLock::new(HyprlandState::default()));

    let connection = Connection::session().await?;
    connection.request_name(args.bus_name.as_str()).await?;
    connection
        .object_server()
        .at(
            args.object_path.as_str(),
            ChromeWindowInfoInterface {
                shared: shared.clone(),
            },
        )
        .await?;

    tokio::spawn(watch_hyprland_events(hyprland.clone()));
    tokio::spawn(process_payloads(
        rx,
        shared,
        connection.clone(),
        args.object_path.clone(),
        hyprland,
    ));

    let path = if args.path.starts_with('/') {
        args.path.clone()
    } else {
        format!("/{}", args.path)
    };
    let leaked_path: &'static str = Box::leak(path.into_boxed_str());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-bridge-token"),
        ]);

    let app = Router::new()
        .route(leaked_path, post(handle_update))
        .with_state(app_state)
        .layer(cors);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(
        "bridge listening on http://{}:{}{}",
        args.host, args.port, leaked_path
    );
    info!("dbus name={} object={}", args.bus_name, args.object_path);

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
