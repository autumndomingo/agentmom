use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use microsandbox::{Sandbox, sandbox::SandboxStatus};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Mutex, task::JoinHandle};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

const OPENCODE_GUEST_PORT: u16 = 4096;

#[derive(Clone)]
struct AppState {
    mom_bin: PathBuf,
    opencode_tunnels: Arc<Mutex<HashMap<String, OpencodeTunnel>>>,
}

struct OpencodeTunnel {
    url: String,
    _sandbox: Sandbox,
    ssh_child: tokio::process::Child,
    server_task: JoinHandle<()>,
    key_dir: PathBuf,
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
    let state = AppState {
        mom_bin,
        opencode_tunnels: Arc::new(Mutex::new(HashMap::new())),
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
        .route("/vms/{name}/opencode", post(opencode_vm))
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

async fn ensure_opencode_tunnel(state: &AppState, name: &str) -> Result<String> {
    {
        let mut tunnels = state.opencode_tunnels.lock().await;
        if let Some(tunnel) = tunnels.get_mut(name) {
            if tunnel_is_healthy(&tunnel.url).await {
                return Ok(tunnel.url.clone());
            }
            let _ = tunnel.ssh_child.kill().await;
            tunnel.server_task.abort();
            let _ = std::fs::remove_dir_all(&tunnel.key_dir);
            tunnels.remove(name);
        }
    }

    let host_port = reserve_host_port().await?;
    let url = format!("http://127.0.0.1:{host_port}");
    let sandbox = running_sandbox_owned(name).await?;
    start_opencode_web(&sandbox).await?;
    let key_dir = env::temp_dir().join(format!("mom-opencode-{}-{}", name, std::process::id()));
    let private_key = key_dir.join("id_ed25519");
    let public_key = key_dir.join("id_ed25519.pub");
    std::fs::create_dir_all(&key_dir).with_context(|| format!("create {}", key_dir.display()))?;
    let keygen = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&private_key)
        .stdin(Stdio::null())
        .output()
        .await
        .context("generate OpenCode tunnel SSH key")?;
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
            &format!("127.0.0.1:{host_port}:127.0.0.1:{OPENCODE_GUEST_PORT}"),
            "-p",
            &ssh_port.to_string(),
            "root@127.0.0.1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(ssh_stderr)
        .spawn()
        .context("start local OpenCode SSH tunnel")?;

    let mut tunnel = OpencodeTunnel {
        url: url.clone(),
        _sandbox: sandbox,
        ssh_child,
        server_task,
        key_dir,
    };
    for _ in 0..50 {
        if tunnel_is_healthy(&url).await {
            state
                .opencode_tunnels
                .lock()
                .await
                .insert(name.to_string(), tunnel);
            return Ok(url);
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
    anyhow::bail!("OpenCode tunnel did not become reachable at {url}; {ssh_status}\n{ssh_log}");
}

async fn tunnel_is_healthy(url: &str) -> bool {
    let Ok(output) = Command::new("curl")
        .args(["-fsS", "--max-time", "2", &format!("{url}/global/health")])
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

async fn start_opencode_web(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
if ! command -v opencode >/dev/null 2>&1; then
  echo "opencode is not installed in this VM; recreate it with the current snapshot" >&2
  exit 1
fi
mkdir -p /workspace /tmp/mom-opencode/bin
cat >/tmp/mom-opencode/bin/xdg-open <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x /tmp/mom-opencode/bin/xdg-open
if wget -q -O /dev/null --timeout=2 http://127.0.0.1:{OPENCODE_GUEST_PORT}/global/health >/dev/null 2>&1; then
  exit 0
fi
cd /workspace
nohup env BROWSER=/bin/true PATH="/tmp/mom-opencode/bin:$PATH" opencode web --hostname 0.0.0.0 --port {OPENCODE_GUEST_PORT} >/tmp/mom-opencode/web.log 2>&1 &
for _ in $(seq 1 60); do
  if wget -q -O /dev/null --timeout=2 http://127.0.0.1:{OPENCODE_GUEST_PORT}/global/health >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
done
cat /tmp/mom-opencode/web.log >&2 || true
exit 1
"#
        ),
    )
    .await
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
