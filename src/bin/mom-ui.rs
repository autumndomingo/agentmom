use std::{
    collections::HashMap, env, net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use microsandbox::{Sandbox, sandbox::SandboxStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{process::Command, sync::Mutex, task::JoinHandle};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

const OPENCODE_GUEST_PORT: u16 = 4096;
const HERMES_GUEST_PORT: u16 = 9119;

#[derive(Clone)]
struct AppState {
    api_url: String,
    client: reqwest::Client,
    opencode_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
    hermes_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
}

struct ServiceTunnel {
    url: String,
    _sandbox: Sandbox,
    ssh_child: tokio::process::Child,
    server_task: JoinHandle<()>,
    key_dir: PathBuf,
}

struct GuestServiceSpec {
    id: &'static str,
    label: &'static str,
    guest_port: u16,
    health_path: &'static str,
    workdir: &'static str,
    log_path: &'static str,
    command: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    pre_start: Option<&'static str>,
    readiness_attempts: u16,
}

const OPENCODE_SERVICE: GuestServiceSpec = GuestServiceSpec {
    id: "opencode",
    label: "OpenCode",
    guest_port: OPENCODE_GUEST_PORT,
    health_path: "/global/health",
    workdir: "/workspace",
    log_path: "/tmp/mom-opencode/web.log",
    command: &[
        "opencode",
        "web",
        "--hostname",
        "0.0.0.0",
        "--port",
        "{port}",
    ],
    env: &[("BROWSER", "/tmp/mom-opencode/bin/xdg-open")],
    pre_start: Some(
        r#"
mkdir -p /tmp/mom-opencode/bin
cat >/tmp/mom-opencode/bin/xdg-open <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x /tmp/mom-opencode/bin/xdg-open
"#,
    ),
    readiness_attempts: 60,
};

const HERMES_SERVICE: GuestServiceSpec = GuestServiceSpec {
    id: "hermes",
    label: "Hermes",
    guest_port: HERMES_GUEST_PORT,
    health_path: "/api/status",
    workdir: "/workspace",
    log_path: "/tmp/mom-hermes/dashboard.log",
    command: &[
        "hermes",
        "dashboard",
        "--host",
        "0.0.0.0",
        "--port",
        "{port}",
        "--no-open",
        "--insecure",
    ],
    env: &[],
    pre_start: None,
    readiness_attempts: 90,
};

#[derive(Debug, Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default = "default_cpus")]
    cpus: u8,
    #[serde(default = "default_memory")]
    memory: u64,
    #[serde(default = "default_volume_quota")]
    volume_quota: u32,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u64,
    #[serde(default = "default_backup_interval")]
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

#[derive(Debug, Deserialize)]
struct WorkspaceRecord {
    name: String,
    status: String,
    desired_state: String,
}

#[derive(Debug, Deserialize)]
struct JobResponse {
    job: JobRecord,
}

#[derive(Debug, Deserialize)]
struct JobRecord {
    id: String,
    status: String,
    output_json: Option<String>,
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

    let api_url = resolve_api_url();
    let port = env::var("MOM_UI_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8787);
    let bind = env::var("MOM_UI_BIND")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], port)));
    let state = AppState {
        api_url,
        client: reqwest::Client::new(),
        opencode_tunnels: Arc::new(Mutex::new(HashMap::new())),
        hermes_tunnels: Arc::new(Mutex::new(HashMap::new())),
    };

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
        .route("/vms/{name}/hermes-ui", post(hermes_ui_vm))
        .route("/vms/{name}/opencode", post(opencode_vm))
        .with_state(state.clone());

    let app = ui_router(api).layer(CorsLayer::permissive());

    println!("Agent Mom UI backend listening on http://{bind}");
    println!("Using Agent Mom API: {}", state.api_url);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
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
) -> Result<Json<ListResponse>, ApiError> {
    let workspaces = state
        .client
        .get(format!("{}/api/workspaces", state.api_url))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<WorkspaceRecord>>()
        .await?;
    let vms = workspaces
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
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(request): Json<CreateRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    let response = state
        .client
        .post(format!("{}/api/workspaces", state.api_url))
        .json(&serde_json::json!({
            "name": request.name,
            "user": request.user,
            "cpus": request.cpus,
            "memory": request.memory,
            "volume_quota": request.volume_quota,
            "idle_timeout": request.idle_timeout,
            "backup_interval": request.backup_interval,
            "node_id": ui_node_id()
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<JobResponse>()
        .await?;
    wait_for_job(&state.api_url, &state.client, &response.job.id).await
}

async fn start_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    create_and_wait_for_job(&state, &name, "start", serde_json::json!({})).await
}

async fn stop_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    create_and_wait_for_job(&state, &name, "stop", serde_json::json!({})).await
}

async fn remove_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    let _ = (state, name);
    Err(ApiError::Anyhow(anyhow::anyhow!(
        "workspace removal is not exposed in the UI yet"
    )))
}

async fn doctor_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    let _ = (state, name);
    Err(ApiError::Anyhow(anyhow::anyhow!(
        "doctor is not exposed in the UI yet"
    )))
}

async fn exec_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    create_and_wait_for_job(
        &state,
        &name,
        "execute",
        serde_json::json!({ "command": request.command }),
    )
    .await
}

async fn codex_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    create_and_wait_for_job(
        &state,
        &name,
        "codex",
        serde_json::json!({ "prompt": request.prompt }),
    )
    .await
}

async fn hermes_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    create_and_wait_for_job(
        &state,
        &name,
        "hermes",
        serde_json::json!({ "args": request.command }),
    )
    .await
}

async fn opencode_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    let url = ensure_opencode_tunnel(&state, &name).await?;
    Ok(Json(CommandResult {
        ok: true,
        code: Some(0),
        stdout: format!("{url}\n"),
        stderr: String::new(),
    }))
}

async fn hermes_ui_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    let url = ensure_hermes_tunnel(&state, &name).await?;
    Ok(Json(CommandResult {
        ok: true,
        code: Some(0),
        stdout: format!("{url}\n"),
        stderr: String::new(),
    }))
}

async fn ensure_opencode_tunnel(state: &AppState, name: &str) -> Result<String> {
    ensure_guest_service_tunnel(state, name, &OPENCODE_SERVICE, &state.opencode_tunnels).await
}

async fn ensure_hermes_tunnel(state: &AppState, name: &str) -> Result<String> {
    ensure_guest_service_tunnel(state, name, &HERMES_SERVICE, &state.hermes_tunnels).await
}

async fn ensure_guest_service_tunnel(
    _state: &AppState,
    name: &str,
    service: &GuestServiceSpec,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    {
        let mut active = tunnels.lock().await;
        if let Some(tunnel) = active.get_mut(name) {
            if tunnel_is_healthy(&tunnel.url, service.health_path).await {
                return Ok(tunnel.url.clone());
            }
            let _ = tunnel.ssh_child.kill().await;
            tunnel.server_task.abort();
            let _ = std::fs::remove_dir_all(&tunnel.key_dir);
            active.remove(name);
        }
    }

    let host_port = reserve_host_port().await?;
    let url = format!("http://127.0.0.1:{host_port}");
    let sandbox = running_sandbox_owned(name).await?;
    ensure_guest_service(&sandbox, service).await?;
    let tunnel = start_service_tunnel(name, service, &sandbox, host_port).await?;
    wait_for_tunnel(name, tunnel, &url, service, tunnels).await
}

async fn start_service_tunnel(
    name: &str,
    service: &GuestServiceSpec,
    sandbox: &Sandbox,
    host_port: u16,
) -> Result<ServiceTunnel> {
    let key_dir = env::temp_dir().join(format!(
        "mom-{}-{}-{}",
        service.id,
        name,
        std::process::id()
    ));
    let private_key = key_dir.join("id_ed25519");
    let public_key = key_dir.join("id_ed25519.pub");
    std::fs::create_dir_all(&key_dir).with_context(|| format!("create {}", key_dir.display()))?;
    let keygen = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&private_key)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("generate {} tunnel SSH key", service.label))?;
    if !keygen.status.success() {
        anyhow::bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&keygen.stderr)
        );
    }
    let public_key_raw = std::fs::read_to_string(&public_key)
        .with_context(|| format!("read {}", public_key.display()))?;
    let authorized_key = public_key_raw
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("parse {}", public_key.display()))?
        .to_string();

    let ssh_server = sandbox
        .ssh()
        .prepare_server_with(|opts| opts.authorized_key(authorized_key).sftp(false))
        .await
        .context("prepare microsandbox SSH tunnel server")?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("bind local microsandbox SSH tunnel server")?;
    let ssh_port = listener
        .local_addr()
        .context("read SSH listener address")?
        .port();
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let server = ssh_server.clone();
            tokio::spawn(async move {
                let _ = server.serve_connection(stream).await;
            });
        }
    });

    let ssh_log_path = key_dir.join("ssh.log");
    let ssh_stderr = std::fs::File::create(&ssh_log_path)
        .with_context(|| format!("create {}", ssh_log_path.display()))?;
    let ssh_child = Command::new("ssh")
        .args(["-F", "/dev/null"])
        .arg("-i")
        .arg(&private_key)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-N",
            "-L",
            &format!("127.0.0.1:{host_port}:127.0.0.1:{}", service.guest_port),
            "-p",
            &ssh_port.to_string(),
            "root@127.0.0.1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(ssh_stderr)
        .spawn()
        .with_context(|| format!("start local {} SSH tunnel", service.label))?;

    Ok(ServiceTunnel {
        url: format!("http://127.0.0.1:{host_port}"),
        _sandbox: sandbox.clone(),
        ssh_child,
        server_task,
        key_dir,
    })
}

async fn wait_for_tunnel(
    name: &str,
    mut tunnel: ServiceTunnel,
    url: &str,
    service: &GuestServiceSpec,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    for _ in 0..50 {
        if tunnel_is_healthy(url, service.health_path).await {
            tunnels.lock().await.insert(name.to_string(), tunnel);
            return Ok(url.to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let _ = tunnel.ssh_child.kill().await;
    tunnel.server_task.abort();
    let ssh_status = tunnel
        .ssh_child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("ssh exited with {status}"))
        .unwrap_or_else(|| "ssh was still running".to_string());
    let ssh_log = std::fs::read_to_string(tunnel.key_dir.join("ssh.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tunnel.key_dir);
    anyhow::bail!(
        "{} tunnel did not become reachable at {url}; {ssh_status}\n{ssh_log}",
        service.label
    );
}

async fn tunnel_is_healthy(url: &str, path: &str) -> bool {
    let Ok(output) = Command::new("curl")
        .args(["-fsS", "--max-time", "2", &format!("{url}{path}")])
        .stdin(Stdio::null())
        .output()
        .await
    else {
        return false;
    };
    output.status.success()
}

async fn running_sandbox_owned(name: &str) -> Result<Sandbox> {
    let handle = Sandbox::get(name)
        .await
        .with_context(|| format!("find sandbox '{name}'"))?;
    match handle.status() {
        SandboxStatus::Running | SandboxStatus::Draining => handle
            .connect_with_timeout(std::time::Duration::from_secs(30))
            .await
            .with_context(|| format!("connect to running sandbox '{name}'")),
        SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Paused => handle
            .start()
            .await
            .with_context(|| format!("start sandbox '{name}'")),
    }
}

async fn ensure_guest_service(sandbox: &Sandbox, service: &GuestServiceSpec) -> Result<()> {
    checked_shell(sandbox, &guest_service_script(service)).await
}

fn guest_service_script(service: &GuestServiceSpec) -> String {
    let executable = service
        .command
        .first()
        .expect("guest service command must not be empty");
    let pre_start = service.pre_start.unwrap_or("");
    let log_dir = service
        .log_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("/tmp");
    let command = guest_service_command(service);

    format!(
        r#"
set -eu
if ! command -v {executable_q} >/dev/null 2>&1; then
  echo "{label} is not installed in this VM; recreate it with the current snapshot" >&2
  exit 1
fi
mkdir -p {workdir_q} {log_dir_q}
{pre_start}
if wget -q -O /dev/null --timeout=2 http://127.0.0.1:{port}{health_path} >/dev/null 2>&1; then
  exit 0
fi
cd {workdir_q}
if ! netstat -ltn 2>/dev/null | grep -q ':{port}[[:space:]]'; then
  setsid sh -c {command_q} &
fi
for _ in $(seq 1 {readiness_attempts}); do
  if wget -q -O /dev/null --timeout=2 http://127.0.0.1:{port}{health_path} >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
done
cat {log_path_q} >&2 || true
exit 1
"#,
        executable_q = shell_quote(executable),
        label = service.label,
        workdir_q = shell_quote(service.workdir),
        log_dir_q = shell_quote(log_dir),
        port = service.guest_port,
        health_path = service.health_path,
        command_q = shell_quote(&command),
        readiness_attempts = service.readiness_attempts,
        log_path_q = shell_quote(service.log_path),
    )
}

fn guest_service_command(service: &GuestServiceSpec) -> String {
    let env = service
        .env
        .iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(value)));
    let argv = service.command.iter().map(|arg| {
        let value = if *arg == "{port}" {
            service.guest_port.to_string()
        } else {
            (*arg).to_string()
        };
        shell_quote(&value)
    });
    env.chain(argv).collect::<Vec<_>>().join(" ")
        + &format!(" </dev/null >{} 2>&1", shell_quote(service.log_path))
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=+".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn checked_shell(sandbox: &Sandbox, script: &str) -> Result<()> {
    let output = sandbox.shell(script).await?;
    if !output.status().success {
        anyhow::bail!(
            "guest shell command exited with {}\n{}\n{}",
            output.status().code,
            output.stdout()?,
            output.stderr()?
        );
    }
    Ok(())
}

async fn reserve_host_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("reserve local OpenCode tunnel port")?;
    let port = listener
        .local_addr()
        .context("read reserved OpenCode tunnel port")?
        .port();
    drop(listener);
    Ok(port)
}

async fn create_and_wait_for_job(
    state: &AppState,
    workspace_name: &str,
    kind: &str,
    payload: Value,
) -> Result<Json<CommandResult>, ApiError> {
    let response = state
        .client
        .post(format!("{}/api/jobs", state.api_url))
        .json(&serde_json::json!({
            "workspace_name": workspace_name,
            "kind": kind,
            "node_id": ui_node_id(),
            "payload": payload
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<JobResponse>()
        .await?;
    wait_for_job(&state.api_url, &state.client, &response.job.id).await
}

async fn wait_for_job(
    api_url: &str,
    client: &reqwest::Client,
    job_id: &str,
) -> Result<Json<CommandResult>, ApiError> {
    for _ in 0..180 {
        let response = client
            .get(format!("{api_url}/api/jobs/{job_id}"))
            .send()
            .await?
            .error_for_status()?
            .json::<JobResponse>()
            .await?;
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
                return Err(ApiError::Command(CommandResult {
                    ok: false,
                    code: Some(1),
                    stdout: String::new(),
                    stderr: job_output_text(response.job.output_json.as_deref()),
                }));
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }

    Err(ApiError::Anyhow(anyhow::anyhow!(
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

fn resolve_api_url() -> String {
    env::var("MOM_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn ui_router(api: Router) -> Router {
    let dist = env::var_os("MOM_UI_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ui/dist"));
    let index = dist.join("index.html");
    if index.exists() {
        let ui = ServeDir::new(dist).fallback(ServeFile::new(index));
        Router::new().nest("/api", api).fallback_service(ui)
    } else {
        Router::new()
            .nest("/api", api)
            .route("/", get(missing_ui))
            .route("/{*path}", get(missing_ui))
    }
}

async fn missing_ui() -> &'static str {
    "Agent Mom UI assets were not found. Set MOM_UI_DIST to a built UI directory."
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

fn default_volume_quota() -> u32 {
    4096
}

fn default_idle_timeout() -> u64 {
    1800
}

fn default_backup_interval() -> u64 {
    0
}

fn ui_node_id() -> Option<String> {
    env::var("MOM_NODE_ID")
        .ok()
        .filter(|value| !value.is_empty())
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

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Anyhow(error.into())
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
