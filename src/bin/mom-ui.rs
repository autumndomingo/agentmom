use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use microsandbox::{Sandbox, sandbox::SandboxStatus};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions, sqlite::SqlitePoolOptions};
use tokio::{process::Command, sync::Mutex, task::JoinHandle};
use tower_http::services::{ServeDir, ServeFile};

const OPENCODE_GUEST_PORT: u16 = 4096;
const ADMIN_EMAIL: &str = "autumndomingo@gmail.com";
const ADMIN_NAME: &str = "Autumn Domingo";
const DEFAULT_ACCESS_CODE: &str = "AD-8KQ-4MZ";
const DEFAULT_ADMIN_ACCESS_CODE: &str = "ADMIN-8KQ-4MZ";

#[derive(Clone)]
struct AppState {
    mom_bin: PathBuf,
    db: SqlitePool,
    admin_email: String,
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

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    access_code: String,
}

#[derive(Debug, Deserialize)]
struct CreateAccessCodeRequest {
    label: Option<String>,
    max_uses: Option<i64>,
    expires_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct UpdateUserRoleRequest {
    role: String,
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
struct AccessConfig {
    admin_email: String,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    ok: bool,
    email: String,
    role: String,
    token: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    ok: bool,
    email: String,
    role: String,
}

#[derive(Debug, Serialize)]
struct UserRecord {
    id: i64,
    name: String,
    email: String,
    role: String,
    status: String,
    last_active_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct UsersResponse {
    users: Vec<UserRecord>,
}

#[derive(Debug, Serialize)]
struct CreateAccessCodeResponse {
    code: String,
    label: String,
    role: String,
    max_uses: Option<i64>,
    expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct LogoutResponse {
    ok: bool,
    logged_out: u64,
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
    let admin_email =
        normalize_email(&env::var("MOM_ADMIN_EMAIL").unwrap_or_else(|_| ADMIN_EMAIL.to_string()));
    let access_code =
        env::var("MOM_ACCESS_CODE").unwrap_or_else(|_| DEFAULT_ACCESS_CODE.to_string());
    let admin_access_code =
        env::var("MOM_ADMIN_ACCESS_CODE").unwrap_or_else(|_| DEFAULT_ADMIN_ACCESS_CODE.to_string());
    let db = open_database().await?;
    migrate_database(&db).await?;
    seed_database(&db, &admin_email, &access_code, &admin_access_code).await?;
    let state = AppState {
        mom_bin,
        db,
        admin_email,
        opencode_tunnels: Arc::new(Mutex::new(HashMap::new())),
    };

    let api = Router::new()
        .route("/health", get(health))
        .route("/auth/config", get(access_config))
        .route("/auth/login", post(login))
        .route("/auth/session", get(current_session))
        .route("/users", get(list_users))
        .route("/users/{id}/role", patch(update_user_role))
        .route("/users/{id}/sessions", delete(log_out_user))
        .route("/sessions", delete(log_out_all_users))
        .route("/access-codes", post(create_access_code))
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
    let app = Router::new().nest("/api", api).fallback_service(ui);

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

async fn access_config(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<AccessConfig> {
    Json(AccessConfig {
        admin_email: state.admin_email,
    })
}

async fn login(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let user = accept_access_code(&state, &request.email, &request.access_code).await?;
    let token = create_session(&state, user.id).await?;
    Ok(Json(LoginResponse {
        ok: true,
        email: user.email,
        role: user.role,
        token,
    }))
}

async fn current_session(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, ApiError> {
    let user = authorize_headers(&state, &headers).await?;
    Ok(Json(SessionResponse {
        ok: true,
        email: user.email,
        role: user.role,
    }))
}

async fn list_users(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
) -> Result<Json<UsersResponse>, ApiError> {
    authorize_admin(&state, &headers).await?;
    let rows = sqlx::query(
        r#"
SELECT id, name, email, role, status, last_active_at
FROM users
ORDER BY
  CASE role WHEN 'ADMN' THEN 0 ELSE 1 END,
  updated_at DESC,
  name ASC
"#,
    )
    .fetch_all(&state.db)
    .await
    .context("list users")?;

    let users = rows
        .into_iter()
        .map(|row| UserRecord {
            id: row.get("id"),
            name: row.get("name"),
            email: row.get("email"),
            role: row.get("role"),
            status: row.get("status"),
            last_active_at: row.get("last_active_at"),
        })
        .collect();

    Ok(Json(UsersResponse { users }))
}

async fn update_user_role(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(request): Json<UpdateUserRoleRequest>,
) -> Result<Json<UserRecord>, ApiError> {
    authorize_admin(&state, &headers).await?;
    let role = request.role.trim().to_uppercase();
    if role != "ADMN" && role != "PAR" {
        return Err(ApiError::BadRequest(
            "Role must be ADMN or PAR.".to_string(),
        ));
    }

    let now = now_epoch();
    let Some(row) = sqlx::query(
        r#"
UPDATE users
SET role = ?1, updated_at = ?2
WHERE id = ?3
RETURNING id, name, email, role, status, last_active_at
"#,
    )
    .bind(role)
    .bind(now)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .context("update user role")?
    else {
        return Err(ApiError::NotFound);
    };

    Ok(Json(UserRecord {
        id: row.get("id"),
        name: row.get("name"),
        email: row.get("email"),
        role: row.get("role"),
        status: row.get("status"),
        last_active_at: row.get("last_active_at"),
    }))
}

async fn log_out_user(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<LogoutResponse>, ApiError> {
    authorize_admin(&state, &headers).await?;
    let exists = sqlx::query("SELECT id FROM users WHERE id = ?1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .context("look up user to log out")?;
    if exists.is_none() {
        return Err(ApiError::NotFound);
    }

    let now = now_epoch();
    let sessions = sqlx::query(
        r#"
UPDATE sessions
SET revoked_at = ?1
WHERE user_id = ?2
  AND revoked_at IS NULL
"#,
    )
    .bind(now)
    .bind(id)
    .execute(&state.db)
    .await
    .context("revoke user sessions")?;

    sqlx::query(
        r#"
UPDATE users
SET status = 'inactive', updated_at = ?1
WHERE id = ?2
"#,
    )
    .bind(now)
    .bind(id)
    .execute(&state.db)
    .await
    .context("mark logged-out user inactive")?;

    Ok(Json(LogoutResponse {
        ok: true,
        logged_out: sessions.rows_affected(),
    }))
}

async fn log_out_all_users(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LogoutResponse>, ApiError> {
    authorize_admin(&state, &headers).await?;
    let now = now_epoch();
    let sessions = sqlx::query(
        r#"
UPDATE sessions
SET revoked_at = ?1
WHERE revoked_at IS NULL
"#,
    )
    .bind(now)
    .execute(&state.db)
    .await
    .context("revoke all sessions")?;

    sqlx::query(
        r#"
UPDATE users
SET status = 'inactive', updated_at = ?1
"#,
    )
    .bind(now)
    .execute(&state.db)
    .await
    .context("mark all users inactive")?;

    Ok(Json(LogoutResponse {
        ok: true,
        logged_out: sessions.rows_affected(),
    }))
}

async fn create_access_code(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateAccessCodeRequest>,
) -> Result<Json<CreateAccessCodeResponse>, ApiError> {
    let admin = authorize_admin(&state, &headers).await?;
    let label = request
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Meetup access")
        .to_string();
    let code = generate_access_code();
    let now = now_epoch();

    sqlx::query(
        r#"
UPDATE access_codes
SET revoked_at = ?1
WHERE role = 'PAR'
  AND revoked_at IS NULL
"#,
    )
    .bind(now)
    .execute(&state.db)
    .await
    .context("revoke previous participant access codes")?;

    sqlx::query(
        r#"
INSERT INTO access_codes (
  code_hash, label, role, max_uses, used_count, created_by_user_id, created_at, expires_at
)
VALUES (?1, ?2, 'PAR', ?3, 0, ?4, ?5, ?6)
"#,
    )
    .bind(access_code_hash(&code))
    .bind(&label)
    .bind(request.max_uses)
    .bind(admin.id)
    .bind(now)
    .bind(request.expires_at)
    .execute(&state.db)
    .await
    .context("create access code")?;

    Ok(Json(CreateAccessCodeResponse {
        code,
        label,
        role: "PAR".to_string(),
        max_uses: request.max_uses,
        expires_at: request.expires_at,
    }))
}

async fn list_vms(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    authorize_headers(&state, &headers).await?;
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
    headers: HeaderMap,
    Json(request): Json<CreateRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
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
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    Ok(Json(run_mom(&state, vec!["start".into(), name]).await?))
}

async fn stop_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    Ok(Json(run_mom(&state, vec!["stop".into(), name]).await?))
}

async fn remove_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    Ok(Json(
        run_mom(&state, vec!["rm".into(), name, "--force".into()]).await?,
    ))
}

async fn doctor_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    Ok(Json(run_mom(&state, vec!["doctor".into(), name]).await?))
}

async fn exec_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    let mut args = vec!["exec".to_string(), name, "--".to_string()];
    args.extend(request.command);
    Ok(Json(run_mom(&state, args).await?))
}

async fn codex_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    Ok(Json(
        run_mom(&state, vec!["codex".into(), name, request.prompt]).await?,
    ))
}

async fn hermes_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
    let mut args = vec!["hermes".to_string(), name, "--".to_string()];
    args.extend(request.command);
    Ok(Json(run_mom(&state, args).await?))
}

async fn opencode_vm(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommandResult>, ApiError> {
    authorize_headers(&state, &headers).await?;
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

#[derive(Debug)]
struct AuthUser {
    id: i64,
    email: String,
    role: String,
}

async fn open_database() -> Result<SqlitePool> {
    let database_url =
        env::var("MOM_DATABASE_URL").unwrap_or_else(|_| "sqlite:agent-mom.db".to_string());
    let options = SqliteConnectOptions::from_str(&database_url)
        .with_context(|| format!("parse MOM_DATABASE_URL={database_url}"))?
        .create_if_missing(true);
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .with_context(|| format!("open SQLite database {database_url}"))
}

async fn migrate_database(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  email TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('ADMN', 'PAR')),
  status TEXT NOT NULL DEFAULT 'active',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_active_at INTEGER
);
"#,
    )
    .execute(db)
    .await
    .context("create users table")?;

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS access_codes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  code_hash TEXT NOT NULL UNIQUE,
  label TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('ADMN', 'PAR')),
  max_uses INTEGER,
  used_count INTEGER NOT NULL DEFAULT 0,
  created_by_user_id INTEGER,
  created_at INTEGER NOT NULL,
  expires_at INTEGER,
  revoked_at INTEGER,
  FOREIGN KEY(created_by_user_id) REFERENCES users(id)
);
"#,
    )
    .execute(db)
    .await
    .context("create access_codes table")?;

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS access_code_uses (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  access_code_id INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  used_at INTEGER NOT NULL,
  FOREIGN KEY(access_code_id) REFERENCES access_codes(id),
  FOREIGN KEY(user_id) REFERENCES users(id)
);
"#,
    )
    .execute(db)
    .await
    .context("create access_code_uses table")?;

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER,
  revoked_at INTEGER,
  FOREIGN KEY(user_id) REFERENCES users(id)
);
"#,
    )
    .execute(db)
    .await
    .context("create sessions table")?;

    Ok(())
}

async fn seed_database(
    db: &SqlitePool,
    admin_email: &str,
    access_code: &str,
    admin_access_code: &str,
) -> Result<()> {
    let now = now_epoch();
    sqlx::query(
        r#"
INSERT INTO users (email, name, role, status, created_at, updated_at, last_active_at)
VALUES (?1, ?2, 'ADMN', 'active', ?3, ?3, ?3)
ON CONFLICT(email) DO UPDATE SET
  role = 'ADMN',
  updated_at = excluded.updated_at
"#,
    )
    .bind(admin_email)
    .bind(ADMIN_NAME)
    .bind(now)
    .execute(db)
    .await
    .context("seed admin user")?;

    let admin_id: i64 = sqlx::query("SELECT id FROM users WHERE email = ?1")
        .bind(admin_email)
        .fetch_one(db)
        .await
        .context("read seeded admin user")?
        .get("id");

    sqlx::query(
        r#"
INSERT INTO access_codes (
  code_hash, label, role, max_uses, used_count, created_by_user_id, created_at
)
VALUES (?1, 'Default meetup access', 'PAR', NULL, 0, ?2, ?3)
ON CONFLICT(code_hash) DO NOTHING
"#,
    )
    .bind(access_code_hash(access_code))
    .bind(admin_id)
    .bind(now)
    .execute(db)
    .await
    .context("seed default open access code")?;

    sqlx::query(
        r#"
INSERT INTO access_codes (
  code_hash, label, role, max_uses, used_count, created_by_user_id, created_at
)
VALUES (?1, 'Admin bootstrap access', 'ADMN', NULL, 0, ?2, ?3)
ON CONFLICT(code_hash) DO NOTHING
"#,
    )
    .bind(access_code_hash(admin_access_code))
    .bind(admin_id)
    .bind(now)
    .execute(db)
    .await
    .context("seed admin access code")?;

    Ok(())
}

async fn authorize_headers(state: &AppState, headers: &HeaderMap) -> Result<AuthUser, ApiError> {
    authorize_session(state, headers)
        .await?
        .ok_or(ApiError::Unauthorized)
}

async fn authorize_admin(state: &AppState, headers: &HeaderMap) -> Result<AuthUser, ApiError> {
    let user = authorize_headers(state, headers).await?;
    if user.role == "ADMN" {
        Ok(user)
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn authorize_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<AuthUser>, ApiError> {
    let Some(token) = bearer_token(headers) else {
        return Ok(None);
    };
    let now = now_epoch();
    let token_hash = session_token_hash(token);
    let Some(row) = sqlx::query(
        r#"
SELECT users.id, users.email, users.role
FROM sessions
JOIN users ON users.id = sessions.user_id
WHERE sessions.token_hash = ?1
  AND sessions.revoked_at IS NULL
  AND (sessions.expires_at IS NULL OR sessions.expires_at > ?2)
  AND users.status = 'active'
"#,
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(&state.db)
    .await
    .context("look up session")?
    else {
        return Err(ApiError::Unauthorized);
    };

    let user_id: i64 = row.get("id");
    sqlx::query(
        r#"
UPDATE users
SET status = 'active', updated_at = ?1, last_active_at = ?1
WHERE id = ?2
"#,
    )
    .bind(now)
    .bind(user_id)
    .execute(&state.db)
    .await
    .context("touch session user")?;

    Ok(Some(AuthUser {
        id: user_id,
        email: row.get("email"),
        role: row.get("role"),
    }))
}

async fn create_session(state: &AppState, user_id: i64) -> Result<String, ApiError> {
    let token = generate_session_token();
    let now = now_epoch();
    sqlx::query(
        r#"
INSERT INTO sessions (user_id, token_hash, created_at)
VALUES (?1, ?2, ?3)
"#,
    )
    .bind(user_id)
    .bind(session_token_hash(&token))
    .bind(now)
    .execute(&state.db)
    .await
    .context("create session")?;
    Ok(token)
}

async fn accept_access_code(
    state: &AppState,
    email: &str,
    access_code: &str,
) -> Result<AuthUser, ApiError> {
    let email = normalize_email(email);
    if !is_valid_email(&email) || access_code.trim().is_empty() {
        return Err(ApiError::Unauthorized);
    }

    let now = now_epoch();
    let code_hash = access_code_hash(access_code);
    let code = sqlx::query(
        r#"
SELECT id, role, max_uses
FROM access_codes
WHERE code_hash = ?1
  AND revoked_at IS NULL
  AND (expires_at IS NULL OR expires_at > ?2)
"#,
    )
    .bind(code_hash)
    .bind(now)
    .fetch_optional(&state.db)
    .await
    .context("look up access code")?
    .ok_or(ApiError::Unauthorized)?;

    let role = code.get::<String, _>("role");
    if role == "ADMN" && !email.eq_ignore_ascii_case(&state.admin_email) {
        return Err(ApiError::Unauthorized);
    }
    if role != "ADMN" && email.eq_ignore_ascii_case(&state.admin_email) {
        return Err(ApiError::Unauthorized);
    }

    let update = sqlx::query(
        r#"
UPDATE access_codes
SET used_count = used_count + 1
WHERE id = ?1
  AND (max_uses IS NULL OR used_count < max_uses)
"#,
    )
    .bind(code.get::<i64, _>("id"))
    .execute(&state.db)
    .await
    .context("increment access code use count")?;
    if update.rows_affected() != 1 {
        return Err(ApiError::Unauthorized);
    }

    let name = email
        .split('@')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("Meetup User");

    sqlx::query(
        r#"
INSERT INTO users (email, name, role, status, created_at, updated_at, last_active_at)
VALUES (?1, ?2, ?3, 'active', ?4, ?4, ?4)
ON CONFLICT(email) DO UPDATE SET
  role = CASE WHEN users.role = 'ADMN' THEN users.role ELSE excluded.role END,
  status = 'active',
  updated_at = excluded.updated_at,
  last_active_at = excluded.last_active_at
"#,
    )
    .bind(&email)
    .bind(name)
    .bind(&role)
    .bind(now)
    .execute(&state.db)
    .await
    .context("create or update access-code user")?;

    let user = sqlx::query("SELECT id, email, role FROM users WHERE email = ?1")
        .bind(&email)
        .fetch_one(&state.db)
        .await
        .context("read access-code user")?;
    let user_id: i64 = user.get("id");

    sqlx::query(
        r#"
INSERT INTO access_code_uses (access_code_id, user_id, used_at)
VALUES (?1, ?2, ?3)
"#,
    )
    .bind(code.get::<i64, _>("id"))
    .bind(user_id)
    .bind(now)
    .execute(&state.db)
    .await
    .context("record access code use")?;

    Ok(AuthUser {
        id: user_id,
        email: user.get("email"),
        role: user.get("role"),
    })
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

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

fn is_valid_email(email: &str) -> bool {
    if email.is_empty() || email.chars().any(char::is_whitespace) {
        return false;
    }

    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }

    let mut labels = domain.split('.');
    let Some(first) = labels.next() else {
        return false;
    };
    if first.is_empty() {
        return false;
    }

    let mut has_tld = false;
    for label in labels {
        if label.is_empty() {
            return false;
        }
        has_tld = true;
    }
    has_tld
}

fn access_code_hash(access_code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_access_code(access_code).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize_access_code(access_code: &str) -> String {
    access_code
        .trim()
        .chars()
        .map(|ch| match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            _ => ch.to_ascii_uppercase(),
        })
        .collect()
}

fn session_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn generate_access_code() -> String {
    format!("MEET-{}", random_code_part(6))
}

fn generate_session_token() -> String {
    format!("session_{}_{}", now_epoch(), random_code_part(32))
}

fn random_code_part(length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..length)
        .map(|_| {
            let index = rng.random_range(0..ALPHABET.len());
            ALPHABET[index] as char
        })
        .collect()
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is before UNIX epoch")
        .as_secs() as i64
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
    Unauthorized,
    Forbidden,
    NotFound,
    BadRequest(String),
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
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorBody {
                    error: "Invalid admin email or access code.".to_string(),
                }),
            )
                .into_response(),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                Json(ErrorBody {
                    error: "Admin access is required.".to_string(),
                }),
            )
                .into_response(),
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ErrorBody {
                    error: "User not found.".to_string(),
                }),
            )
                .into_response(),
            ApiError::BadRequest(error) => {
                (StatusCode::BAD_REQUEST, Json(ErrorBody { error })).into_response()
            }
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
