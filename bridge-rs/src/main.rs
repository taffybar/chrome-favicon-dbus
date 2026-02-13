use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};
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
}

struct ChromeWindowInfoInterface {
    shared: SharedDbusState,
}

#[interface(name = "org.imalison.ChromeWindowInfo")]
impl ChromeWindowInfoInterface {
    async fn get_last_payload(&self) -> String {
        self.shared.last_payload.read().await.clone()
    }

    async fn get_schema(&self) -> String {
        self.shared.schema.clone()
    }

    #[zbus(signal)]
    async fn updated(ctxt: &SignalContext<'_>, payload: &str) -> zbus::Result<()>;
}

#[derive(Default)]
struct PayloadEnricher {
    window_map: HashMap<String, String>,
}

impl PayloadEnricher {
    fn enrich(&mut self, mut payload: Value) -> Value {
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

        let wm = build_wm_context(&tab_title);

        let active = wm.get("hyprland_active").and_then(Value::as_object);
        let active_class = active
            .and_then(|h| h.get("class"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let active_address = active
            .and_then(|h| h.get("address"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let mapped_window = if let (Some(chrome_id), Some(active_addr)) =
            (chrome_window_id.clone(), active_address)
        {
            if chrome_window_focused && looks_like_chrome(active_class) {
                self.window_map.insert(chrome_id, active_addr.clone());
                Some(json!({
                    "backend": "hyprland",
                    "window_id": active_addr,
                    "source": "hyprland_active_window"
                }))
            } else if let Some(cached) = self.window_map.get(&chrome_id) {
                Some(json!({
                    "backend": "hyprland",
                    "window_id": cached,
                    "source": "cached_mapping"
                }))
            } else {
                None
            }
        } else {
            None
        };

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
) {
    let mut enricher = PayloadEnricher::default();

    while let Some(payload) = rx.recv().await {
        let enriched = enricher.enrich(payload);
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

        let ctxt = match SignalContext::new(&connection, object_path.as_str()) {
            Ok(v) => v,
            Err(err) => {
                error!("failed to build dbus signal context: {err}");
                continue;
            }
        };

        if let Err(err) = ChromeWindowInfoInterface::updated(&ctxt, &payload_text).await {
            error!("failed to emit Updated signal: {err}");
        }
    }
}

fn build_wm_context(tab_title: &str) -> Map<String, Value> {
    let mut context = Map::new();
    context.insert("checked_at".to_owned(), Value::String(utc_now_iso()));

    let mut available_backends = Vec::new();

    if command_exists("hyprctl") {
        available_backends.push(Value::String("hyprland".to_owned()));

        if let Some(active) = run_json_command("hyprctl", &["-j", "activewindow"]) {
            if active.is_object() && !active.as_object().is_some_and(Map::is_empty) {
                context.insert("hyprland_active".to_owned(), compact_hypr_client(&active));
            }
        }

        if !tab_title.is_empty() {
            if let Some(clients) = run_json_command("hyprctl", &["-j", "clients"]) {
                if let Some(client_list) = clients.as_array() {
                    let title_lower = tab_title.to_lowercase();
                    let mut matches = Vec::new();

                    for client in client_list {
                        let class = client
                            .get("class")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !looks_like_chrome(class) {
                            continue;
                        }

                        let client_title = client
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_default();

                        if client_title.to_lowercase().contains(&title_lower) {
                            matches.push(compact_hypr_client(client));
                        }

                        if matches.len() >= 5 {
                            break;
                        }
                    }

                    context.insert("hyprland_title_matches".to_owned(), Value::Array(matches));
                }
            }
        }
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

fn compact_hypr_client(client: &Value) -> Value {
    let mut obj = Map::new();

    insert_optional(&mut obj, "address", client.get("address"));
    insert_optional(&mut obj, "class", client.get("class"));
    insert_optional(&mut obj, "title", client.get("title"));
    insert_optional(&mut obj, "pid", client.get("pid"));

    let workspace_id = client
        .get("workspace")
        .and_then(Value::as_object)
        .and_then(|workspace| workspace.get("id"));
    insert_optional(&mut obj, "workspace_id", workspace_id);

    Value::Object(obj)
}

fn insert_optional(target: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        target.insert(key.to_owned(), v.clone());
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

fn run_json_command(command: &str, args: &[&str]) -> Option<Value> {
    let text = run_text_command(command, args)?;
    serde_json::from_str::<Value>(&text).ok()
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

    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
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
    };

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

    tokio::spawn(process_payloads(
        rx,
        shared,
        connection.clone(),
        args.object_path.clone(),
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
        .allow_headers([header::CONTENT_TYPE, header::HeaderName::from_static("x-bridge-token")]);

    let app = Router::new()
        .route(leaked_path, post(handle_update))
        .with_state(app_state)
        .layer(cors);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("bridge listening on http://{}:{}{}", args.host, args.port, leaked_path);
    info!("dbus name={} object={}", args.bus_name, args.object_path);

    axum::serve(listener, app).await?;
    Ok(())
}
