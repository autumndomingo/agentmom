use std::{env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    ApiState, JobResponse, WorkspaceRecord, create_job, default_workspace_backup_interval,
    default_workspace_cpus, default_workspace_idle_timeout, default_workspace_memory,
    default_workspace_volume_quota,
    job_search::{JobSearchRequest, JobSearchResponse, search_jobs},
    job_get, node_worker_url, sanitize_workspace_name, select_ready_node, worker_token,
    workspace_all, workspace_get, workspace_upsert_pending,
};

#[derive(Debug, Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default = "default_workspace_cpus")]
    cpus: u8,
    #[serde(default = "default_workspace_memory")]
    memory: u64,
    #[serde(default = "default_workspace_volume_quota")]
    volume_quota: u32,
    #[serde(default = "default_workspace_idle_timeout")]
    idle_timeout: u64,
    #[serde(default = "default_workspace_backup_interval")]
    backup_interval: u64,
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
        .route("/api/vms", get(list_vms).post(create_vm))
        .route("/api/vms/{name}/start", post(start_vm))
        .route("/api/vms/{name}/stop", post(stop_vm))
        .route("/api/vms/{name}/remove", post(remove_vm))
        .route("/api/vms/{name}/doctor", post(doctor_vm))
        .route("/api/vms/{name}/exec", post(exec_vm))
        .route("/api/vms/{name}/codex", post(codex_vm))
        .route("/api/vms/{name}/hermes", post(hermes_vm))
        .route("/api/vms/{name}/hermes-ui", post(hermes_ui_vm))
        .route("/api/vms/{name}/opencode", post(opencode_vm))
        .route("/api/opportunity-search", post(opportunity_search))
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

async fn list_vms() -> Result<Json<ListResponse>, UiError> {
    let vms = workspace_all()?
        .into_iter()
        .map(|workspace| Vm {
            name: workspace.name,
            status: workspace.status,
            image: workspace.desired_state,
        })
        .collect();
    Ok(Json(ListResponse {
        vms,
        raw: CommandResult {
            ok: true,
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        },
    }))
}

async fn create_vm(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateRequest>,
) -> Result<Json<CommandResult>, UiError> {
    let name = sanitize_workspace_name(&request.name)?;
    if workspace_get(&name).is_ok() {
        return Err(UiError::Anyhow(anyhow!("workspace already exists: {name}")));
    }
    let node_id = select_ready_node(None)?;
    let memory =
        u32::try_from(request.memory).map_err(|_| anyhow!("memory must fit in u32 MiB"))?;
    let user_id = request.user.clone().unwrap_or_else(|| name.clone());
    workspace_upsert_pending(
        &name,
        &user_id,
        &format!("mom-{name}"),
        &format!("mom-{name}-workspace"),
        Some(&node_id),
        request.cpus,
        memory,
        request.volume_quota,
        request.idle_timeout,
        request.backup_interval,
    )?;
    let job = create_job(crate::CreateJobRequest {
        workspace_name: name,
        kind: "create".to_string(),
        node_id: Some(node_id),
        payload: json!({
            "user": request.user,
            "cpus": request.cpus,
            "memory": request.memory,
            "volume_quota": request.volume_quota,
            "idle_timeout": request.idle_timeout,
            "backup_interval": request.backup_interval
        }),
    })?;
    let _ = state.notifier.send("job_available".to_string());
    wait_for_job(&job.id).await
}

async fn start_vm(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    create_and_wait_for_job(&state, &name, "start", json!({})).await
}

async fn stop_vm(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, UiError> {
    create_and_wait_for_job(&state, &name, "stop", json!({})).await
}

async fn remove_vm(Path(name): Path<String>) -> Result<Json<CommandResult>, UiError> {
    let _ = name;
    Err(UiError::Anyhow(anyhow!(
        "workspace removal is not exposed in the UI yet"
    )))
}

async fn doctor_vm(Path(name): Path<String>) -> Result<Json<CommandResult>, UiError> {
    let _ = name;
    Err(UiError::Anyhow(anyhow!(
        "doctor is not exposed in the UI yet"
    )))
}

async fn exec_vm(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, UiError> {
    create_and_wait_for_job(
        &state,
        &name,
        "execute",
        json!({ "command": request.command }),
    )
    .await
}

async fn codex_vm(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<CommandResult>, UiError> {
    create_and_wait_for_job(&state, &name, "codex", json!({ "prompt": request.prompt })).await
}

async fn hermes_vm(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, UiError> {
    create_and_wait_for_job(&state, &name, "hermes", json!({ "args": request.command })).await
}

async fn opencode_vm(Path(name): Path<String>) -> Result<Json<CommandResult>, UiError> {
    open_workspace_service(&name, "opencode").await
}

async fn hermes_ui_vm(Path(name): Path<String>) -> Result<Json<CommandResult>, UiError> {
    open_workspace_service(&name, "hermes").await
}

async fn opportunity_search(
    Json(request): Json<JobSearchRequest>,
) -> Result<Json<JobSearchResponse>, UiError> {
    Ok(Json(search_jobs(request).await?))
}

async fn open_workspace_service(name: &str, service: &str) -> Result<Json<CommandResult>, UiError> {
    let workspace = workspace_get(name)?;
    let worker_url = workspace_worker_url(&workspace)?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .post(format!(
            "{}/worker/services/{service}/open",
            worker_url.trim_end_matches('/')
        ))
        .with_worker_token()
        .json(&OpenWorkerServiceRequest {
            workspace_name: workspace.name,
            sandbox_name: workspace.sandbox_name,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<OpenWorkerServiceResponse>()
        .await?;
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

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        match self {
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
