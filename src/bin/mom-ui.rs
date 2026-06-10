use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    process::{ExitStatus, Stdio},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

#[derive(Clone)]
struct AppState {
    mom_bin: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    replace: bool,
    #[serde(default = "default_cpus")]
    cpus: u8,
    #[serde(default = "default_memory")]
    memory: u64,
    #[serde(default)]
    rebuild_snapshot: bool,
    #[serde(default)]
    no_snapshot: bool,
}

#[derive(Debug, Deserialize)]
struct CommandRequest {
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PromptRequest {
    prompt: String,
}

#[derive(Debug, Serialize)]
struct Vm {
    name: String,
    status: String,
    image: String,
}

#[derive(Debug, Serialize)]
struct CommandResult {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    vms: Vec<Vm>,
    raw: CommandResult,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mom_ui=info,tower_http=info".into()),
        )
        .init();

    let mom_bin = resolve_mom_bin()?;
    let port = env::var("MOM_UI_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8787);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let state = AppState { mom_bin };

    let api = Router::new()
        .route("/health", get(health))
        .route("/vms", get(list_vms).post(create_vm))
        .route("/vms/{name}/start", post(start_vm))
        .route("/vms/{name}/stop", post(stop_vm))
        .route("/vms/{name}/remove", post(remove_vm))
        .route("/vms/{name}/doctor", post(doctor_vm))
        .route("/vms/{name}/exec", post(exec_vm))
        .route("/vms/{name}/codex", post(codex_vm))
        .route("/vms/{name}/hermes", post(hermes_vm))
        .with_state(state);

    let ui = ServeDir::new("ui/dist").fallback(ServeFile::new("ui/dist/index.html"));
    let app = Router::new()
        .nest("/api", api)
        .fallback_service(ui)
        .layer(CorsLayer::permissive());

    println!("Agent Mom UI backend listening on http://{addr}");
    println!("Using mom binary: {}", resolve_mom_bin()?.display());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn list_vms(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let mut args = vec!["list".to_string()];
    if query.all.unwrap_or(false) {
        args.push("--all".to_string());
    }
    let raw = run_mom(&state, args).await?;
    let vms = parse_vms(&raw.stdout);
    Ok(Json(ListResponse { vms, raw }))
}

async fn create_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(request): Json<CreateRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    let mut args = vec![
        "create".to_string(),
        request.name,
        "--cpus".to_string(),
        request.cpus.to_string(),
        "--memory".to_string(),
        request.memory.to_string(),
    ];
    if request.replace {
        args.push("--replace".to_string());
    }
    if request.rebuild_snapshot {
        args.push("--rebuild-snapshot".to_string());
    }
    if request.no_snapshot {
        args.push("--no-snapshot".to_string());
    }
    Ok(Json(run_mom(&state, args).await?))
}

async fn start_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    Ok(Json(run_mom(&state, vec!["start".into(), name]).await?))
}

async fn stop_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    Ok(Json(run_mom(&state, vec!["stop".into(), name]).await?))
}

async fn remove_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    Ok(Json(
        run_mom(&state, vec!["rm".into(), name, "--force".into()]).await?,
    ))
}

async fn doctor_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    Ok(Json(run_mom(&state, vec!["doctor".into(), name]).await?))
}

async fn exec_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    let mut args = vec!["exec".to_string(), name, "--".to_string()];
    args.extend(request.command);
    Ok(Json(run_mom(&state, args).await?))
}

async fn codex_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    Ok(Json(
        run_mom(&state, vec!["codex".into(), name, request.prompt]).await?,
    ))
}

async fn hermes_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    let mut args = vec!["hermes".to_string(), name, "--".to_string()];
    args.extend(request.command);
    Ok(Json(run_mom(&state, args).await?))
}

async fn run_mom(state: &AppState, args: Vec<String>) -> Result<CommandResult, ApiError> {
    let output = Command::new(&state.mom_bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {}", state.mom_bin.display()))?;

    let result = command_result(output.status, output.stdout, output.stderr);
    if result.ok {
        Ok(result)
    } else {
        Err(ApiError::Command(result))
    }
}

fn command_result(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> CommandResult {
    CommandResult {
        ok: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    }
}

fn parse_vms(stdout: &str) -> Vec<Vm> {
    stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?.to_string();
            let status = parts.next()?.to_string();
            let image = parts.collect::<Vec<_>>().join(" ");
            Some(Vm {
                name,
                status,
                image,
            })
        })
        .collect()
}

fn resolve_mom_bin() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_BIN") {
        return Ok(PathBuf::from(path));
    }

    let current = env::current_exe().context("resolve current executable")?;
    if let Some(parent) = current.parent() {
        let candidate = parent.join("mom");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(PathBuf::from("mom"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn default_cpus() -> u8 {
    2
}

fn default_memory() -> u64 {
    2048
}

enum ApiError {
    Anyhow(anyhow::Error),
    Command(CommandResult),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Anyhow(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Anyhow(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: format!("{error:#}"),
                }),
            )
                .into_response(),
            ApiError::Command(result) => (StatusCode::BAD_REQUEST, Json(result)).into_response(),
        }
    }
}
