use std::{collections::HashMap, env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, State, ws::Message},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message as WorkerMessage,
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    ApiState, JobResponse, WorkspaceRecord, create_job, job_get, node_worker_url,
    service_tunnel_hostname_registered, service_tunnel_upsert, worker_token_for_node,
    workspace_get,
};

const UI_JOB_WAIT_TIMEOUT: Duration = Duration::from_secs(360);
const UI_WORKER_SERVICE_TIMEOUT: Duration = Duration::from_secs(360);

#[derive(Debug, Deserialize)]
struct CommandRequest {
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CommandResult {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
struct OpenWorkerServiceRequest {
    workspace_name: String,
    vm_name: String,
}

#[derive(Debug, Deserialize)]
struct OpenWorkerServiceResponse {
    url: String,
}

pub(crate) fn api_routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/api/ui/health", get(health))
        .route("/api/tls-ask", get(tls_ask))
        .route("/api/workspaces/{name}/start", post(start_workspace))
        .route("/api/workspaces/{name}/stop", post(stop_workspace))
        .route("/api/workspaces/{name}/remove", post(remove_workspace))
        .route("/api/workspaces/{name}/doctor", post(doctor_workspace))
        .route("/api/workspaces/{name}/exec", post(exec_workspace))
        .route("/api/workspaces/{name}/hermes", post(hermes_workspace))
        .route(
            "/api/workspaces/{name}/hermes-ui",
            post(hermes_ui_workspace),
        )
        .route("/api/workspaces/{name}/chat/ws", get(chat_ws))
}

pub(crate) fn serve_assets(app: Router) -> Router {
    let dist = env::var_os("MOM_UI_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ui/dist"));
    let index = dist.join("index.html");
    if index.exists() {
        let ui = ServeDir::new(dist).fallback(ServeFile::new(index));
        app.fallback_service(ui)
    } else {
        app.route("/", get(missing_ui))
            .route("/{*path}", get(missing_ui))
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn tls_ask(Query(query): Query<HashMap<String, String>>) -> StatusCode {
    let Some(domain) = query.get("domain") else {
        return StatusCode::BAD_REQUEST;
    };
    if service_tunnel_domain_allowed(domain) && service_tunnel_registered(domain).unwrap_or(false) {
        StatusCode::OK
    } else {
        StatusCode::FORBIDDEN
    }
}

fn service_tunnel_domain_allowed(domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    let Some(label) = domain.strip_suffix(".agentmom.xyz") else {
        return false;
    };

    ["mom-1-", "mom-2-"].iter().any(|prefix| {
        let Some(port) = label.strip_prefix(prefix) else {
            return false;
        };
        !port.is_empty()
            && port.parse::<u16>().is_ok()
            && port.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn service_tunnel_registered(domain: &str) -> Result<bool> {
    service_tunnel_hostname_registered(domain)
}

async fn start_workspace(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    create_and_wait_for_job(&state, &name, "start", json!({})).await
}

async fn stop_workspace(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    create_and_wait_for_job(&state, &name, "stop", json!({})).await
}

async fn remove_workspace(
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::require_admin(&headers)?;
    let _ = name;
    Err(UiError::Anyhow(anyhow!(
        "workspace removal is not exposed in the UI yet"
    )))
}

async fn doctor_workspace(
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    let _ = name;
    Err(UiError::Anyhow(anyhow!(
        "doctor is not exposed in the UI yet"
    )))
}

async fn exec_workspace(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    create_and_wait_for_job(
        &state,
        &name,
        "execute",
        json!({ "command": request.command }),
    )
    .await
}

async fn hermes_workspace(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    create_and_wait_for_job(&state, &name, "hermes", json!({ "args": request.command })).await
}

async fn hermes_ui_workspace(
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    open_hermes_dashboard(&name).await
}

async fn chat_ws(
    headers: HeaderMap,
    Path(name): Path<String>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<Response, UiError> {
    crate::auth::authorize_workspace(&headers, &name)?;
    let workspace = workspace_get(&name)?;
    let worker_url = workspace_worker_url(&workspace)?;
    let worker_ws = worker_acp_ws_url(&worker_url, &workspace)?;
    let worker_token = worker_token_for_node(workspace.node_id.as_deref().ok_or_else(|| {
        anyhow!(
            "workspace {} does not have an assigned node",
            workspace.name
        )
    })?)?;
    Ok(ws
        .on_upgrade(move |socket| async move {
            let _ = proxy_acp_websocket(socket, worker_ws, worker_token).await;
        })
        .into_response())
}

fn worker_acp_ws_url(worker_url: &str, workspace: &WorkspaceRecord) -> Result<String> {
    let mut url = worker_url.trim_end_matches('/').to_string();
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    } else if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    } else if !url.starts_with("ws://") && !url.starts_with("wss://") {
        bail!("worker_url must start with http://, https://, ws://, or wss://");
    }
    Ok(format!(
        "{url}/worker/hermes-acp/ws?workspace_name={}&vm_name={}",
        crate::url_component(&workspace.name),
        crate::url_component(&workspace.vm_name)
    ))
}

async fn proxy_acp_websocket(
    mut browser_socket: axum::extract::ws::WebSocket,
    worker_ws_url: String,
    token: String,
) -> Result<()> {
    let mut request = worker_ws_url.into_client_request()?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let worker_socket = match connect_async(request).await {
        Ok((socket, _)) => socket,
        Err(error) => {
            let _ = send_acp_status(
                &mut browser_socket,
                "error",
                &format!("Hermes ACP worker websocket failed: {error}"),
            )
            .await;
            return Err(error.into());
        }
    };
    let (mut browser_tx, mut browser_rx) = browser_socket.split();
    let (mut worker_tx, mut worker_rx) = worker_socket.split();

    let browser_to_worker = async {
        while let Some(Ok(message)) = browser_rx.next().await {
            let message = match message {
                Message::Text(text) => WorkerMessage::Text(text.to_string().into()),
                Message::Binary(bytes) => WorkerMessage::Binary(bytes),
                Message::Ping(bytes) => WorkerMessage::Ping(bytes),
                Message::Pong(bytes) => WorkerMessage::Pong(bytes),
                Message::Close(frame) => {
                    let close =
                        frame.map(
                            |frame| tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: frame.code.into(),
                                reason: frame.reason.to_string().into(),
                            },
                        );
                    let _ = worker_tx.send(WorkerMessage::Close(close)).await;
                    break;
                }
            };
            if worker_tx.send(message).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let worker_to_browser = async {
        while let Some(Ok(message)) = worker_rx.next().await {
            let message = match message {
                WorkerMessage::Text(text) => Message::Text(text.to_string().into()),
                WorkerMessage::Binary(bytes) => Message::Binary(bytes),
                WorkerMessage::Ping(bytes) => Message::Ping(bytes),
                WorkerMessage::Pong(bytes) => Message::Pong(bytes),
                WorkerMessage::Close(frame) => {
                    let close = frame.map(|frame| axum::extract::ws::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.to_string().into(),
                    });
                    let _ = browser_tx.send(Message::Close(close)).await;
                    break;
                }
                WorkerMessage::Frame(_) => continue,
            };
            if browser_tx.send(message).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = browser_to_worker => result?,
        result = worker_to_browser => result?,
    }
    Ok(())
}

async fn send_acp_status(
    socket: &mut axum::extract::ws::WebSocket,
    state: &str,
    message: &str,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/status",
                "params": {
                    "state": state,
                    "message": message,
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| anyhow!("send Hermes ACP websocket status: {error}"))
}

async fn open_hermes_dashboard(name: &str) -> Result<Json<CommandResult>, UiError> {
    let workspace = workspace_get(name)?;
    let worker_url = workspace_worker_url(&workspace)?;
    let workspace_name = workspace.name.clone();
    let vm_name = workspace.vm_name.clone();
    let node = workspace.node_id.as_deref().ok_or_else(|| {
        anyhow!(
            "workspace {} does not have an assigned node",
            workspace.name
        )
    })?;
    let token = worker_token_for_node(node)?;
    let response = reqwest::Client::builder()
        .timeout(UI_WORKER_SERVICE_TIMEOUT)
        .build()?
        .post(format!(
            "{}/worker/services/hermes/open",
            worker_url.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .json(&OpenWorkerServiceRequest {
            workspace_name: workspace_name.clone(),
            vm_name,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<OpenWorkerServiceResponse>()
        .await?;
    service_tunnel_upsert(&workspace_name, node, "hermes", &response.url)?;
    Ok(Json(CommandResult {
        ok: true,
        code: Some(0),
        stdout: format!("{}\n", response.url),
        stderr: String::new(),
    }))
}

fn workspace_worker_url(workspace: &WorkspaceRecord) -> Result<String> {
    let node = workspace.node_id.as_deref().ok_or_else(|| {
        anyhow!(
            "workspace {} does not have an assigned node",
            workspace.name
        )
    })?;
    node_worker_url(node)?.ok_or_else(|| anyhow!("node {node} has not registered a worker_url"))
}

async fn create_and_wait_for_job(
    state: &ApiState,
    workspace_name: &str,
    kind: &str,
    payload: Value,
) -> Result<Json<CommandResult>, UiError> {
    let workspace = workspace_get(workspace_name)?;
    let node_id = workspace.node_id.as_deref().ok_or_else(|| {
        anyhow!(
            "workspace {} does not have an assigned node",
            workspace.name
        )
    })?;
    crate::require_claimable_node(node_id)?;
    let job = create_job(crate::CreateJobRequest {
        workspace_name: workspace_name.to_string(),
        kind: kind.to_string(),
        node_id: workspace.node_id,
        payload,
    })?;
    let _ = state.notifier.send("job_available".to_string());
    wait_for_job(&job.id).await
}

async fn wait_for_job(job_id: &str) -> Result<Json<CommandResult>, UiError> {
    let deadline = tokio::time::Instant::now() + UI_JOB_WAIT_TIMEOUT;
    loop {
        let response = JobResponse {
            job: job_get(job_id)?,
        };
        match response.job.status.as_str() {
            "succeeded" => {
                return Ok(Json(CommandResult {
                    ok: true,
                    code: Some(0),
                    stdout: job_output_text(response.job.output_json.as_deref()),
                    stderr: String::new(),
                }));
            }
            "failed" | "canceled" => {
                return Err(UiError::Command(CommandResult {
                    ok: false,
                    code: Some(1),
                    stdout: String::new(),
                    stderr: job_output_text(response.job.output_json.as_deref()),
                }));
            }
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            _ => break,
        }
    }

    Err(UiError::Anyhow(anyhow!(
        "timed out waiting for job {job_id}"
    )))
}

fn job_output_text(output_json: Option<&str>) -> String {
    let Some(raw) = output_json else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        let stderr = value.get("stderr").and_then(Value::as_str).unwrap_or("");
        return [stdout, stderr]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    value.to_string()
}

async fn missing_ui() -> &'static str {
    "Agent Mom UI assets were not found. Set MOM_UI_DIST to a built UI directory."
}

enum UiError {
    Anyhow(anyhow::Error),
    Auth(crate::auth::AuthError),
    Command(CommandResult),
}

impl From<anyhow::Error> for UiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Anyhow(error)
    }
}

impl From<reqwest::Error> for UiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Anyhow(error.into())
    }
}

impl From<crate::auth::AuthError> for UiError {
    fn from(error: crate::auth::AuthError) -> Self {
        Self::Auth(error)
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        match self {
            UiError::Auth(error) => error.into_response(),
            UiError::Anyhow(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: format!("{error:#}"),
                }),
            )
                .into_response(),
            UiError::Command(result) => {
                let status = if result.ok {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status, Json(result)).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::service_tunnel_domain_allowed;

    #[test]
    fn service_tunnel_domain_allows_known_node_port_hosts() {
        assert!(service_tunnel_domain_allowed("mom-1-45887.agentmom.xyz"));
        assert!(service_tunnel_domain_allowed("mom-2-45887.agentmom.xyz"));
    }

    #[test]
    fn service_tunnel_domain_rejects_unexpected_hosts() {
        assert!(!service_tunnel_domain_allowed("agentmom.xyz"));
        assert!(!service_tunnel_domain_allowed("mom-3-45887.agentmom.xyz"));
        assert!(!service_tunnel_domain_allowed("mom-1-api.agentmom.xyz"));
        assert!(!service_tunnel_domain_allowed("mom-1-45887.example.com"));
    }
}
