use std::{collections::HashMap, env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    ApiState, JobResponse, WorkspaceRecord, create_job, job_get, node_worker_url,
    service_tunnel_hostname_registered, service_tunnel_upsert, worker_token, workspace_get,
};

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
    sandbox_name: String,
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

async fn open_hermes_dashboard(name: &str) -> Result<Json<CommandResult>, UiError> {
    let workspace = workspace_get(name)?;
    let worker_url = workspace_worker_url(&workspace)?;
    let workspace_name = workspace.name.clone();
    let sandbox_name = workspace.sandbox_name.clone();
    let node = workspace.node_id.as_deref().ok_or_else(|| {
        anyhow!(
            "workspace {} does not have an assigned node",
            workspace.name
        )
    })?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?
        .post(format!(
            "{}/worker/services/hermes/open",
            worker_url.trim_end_matches('/')
        ))
        .with_worker_token()
        .json(&OpenWorkerServiceRequest {
            workspace_name: workspace_name.clone(),
            sandbox_name,
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
    for _ in 0..180 {
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
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
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

trait WorkerTokenExt {
    fn with_worker_token(self) -> Self;
}

impl WorkerTokenExt for reqwest::RequestBuilder {
    fn with_worker_token(self) -> Self {
        match worker_token() {
            Ok(token) if !token.trim().is_empty() => self.bearer_auth(token),
            _ => self,
        }
    }
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
