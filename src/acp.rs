use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use microsandbox::Sandbox;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

const EVENT_CAP: usize = 2000;
const FAILED_RETRY_AFTER_MS: u128 = 10_000;

#[derive(Clone, Default)]
pub(crate) struct HermesAcpState {
    sessions: Arc<Mutex<HashMap<String, AcpEntry>>>,
}

enum AcpEntry {
    Running(AcpSession),
    Failed(AcpFailure),
}

struct AcpSession {
    workspace_name: String,
    session_id: Option<String>,
    initialized: bool,
    phase: String,
    last_error: Option<String>,
    next_id: u64,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<JsonRpcId, oneshot::Sender<Value>>>>,
    events: Arc<Mutex<Vec<AcpEvent>>>,
    pending_permissions: Arc<Mutex<HashMap<String, PendingPermission>>>,
    exited: Arc<Mutex<bool>>,
    _sandbox: Sandbox,
    ssh_child: tokio::process::Child,
    server_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    key_dir: PathBuf,
    ssh_log_path: PathBuf,
}

struct AcpFailure {
    workspace_name: String,
    initialized: bool,
    phase: String,
    last_error: String,
    last_attempt_ms: u128,
    events: Arc<Mutex<Vec<AcpEvent>>>,
    pending_permissions: Arc<Mutex<HashMap<String, PendingPermission>>>,
}

struct AcpCallTarget {
    id: JsonRpcId,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<JsonRpcId, oneshot::Sender<Value>>>>,
    events: Arc<Mutex<Vec<AcpEvent>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JsonRpcId {
    Number(u64),
    String(String),
}

impl JsonRpcId {
    fn from_value(value: &Value) -> Option<Self> {
        if let Some(id) = value.as_u64() {
            return Some(Self::Number(id));
        }
        value.as_str().map(|id| Self::String(id.to_string()))
    }

    fn as_value(&self) -> Value {
        match self {
            Self::Number(id) => Value::from(*id),
            Self::String(id) => Value::String(id.clone()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct AcpEvent {
    seq: u64,
    at_ms: u128,
    direction: String,
    kind: String,
    payload: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct PendingPermission {
    id: String,
    method: String,
    params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpStartRequest {
    #[serde(default)]
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) sandbox_name: String,
    #[serde(default)]
    pub(crate) restart: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpSendRequest {
    #[serde(default)]
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) sandbox_name: String,
    #[serde(default)]
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) content: Vec<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpCancelRequest {
    #[serde(default)]
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) sandbox_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpPermissionRequest {
    #[serde(default)]
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) sandbox_name: String,
    pub(crate) request_id: String,
    pub(crate) option_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpEventsQuery {
    #[serde(default)]
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) sandbox_name: String,
    pub(crate) after: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AcpStatusResponse {
    workspace: String,
    session_id: Option<String>,
    initialized: bool,
    state: String,
    phase: String,
    error: Option<String>,
    events: Vec<AcpEvent>,
    pending_permissions: Vec<PendingPermission>,
    capabilities: Option<Value>,
}

pub(crate) async fn ensure_session(
    state: &HermesAcpState,
    workspace_name: &str,
    sandbox_name: &str,
    sandbox: &Sandbox,
    restart: bool,
) -> Result<AcpStatusResponse> {
    let now = now_ms();
    let mut old = None;
    {
        let mut sessions = state.sessions.lock().await;
        if restart {
            old = sessions.remove(workspace_name);
        } else if let Some(entry) = sessions.get(workspace_name) {
            match entry {
                AcpEntry::Running(session) => {
                    if !*session.exited.lock().await {
                        drop(sessions);
                        return status(state, workspace_name, 0).await;
                    }
                }
                AcpEntry::Failed(failure)
                    if now.saturating_sub(failure.last_attempt_ms) < FAILED_RETRY_AFTER_MS =>
                {
                    drop(sessions);
                    return status(state, workspace_name, 0).await;
                }
                AcpEntry::Failed(_) => {}
            }
            old = sessions.remove(workspace_name);
        }
    }
    if let Some(AcpEntry::Running(session)) = old {
        cleanup_session(session).await;
    }

    if let Err(error) = preflight_hermes(sandbox).await {
        let events = Arc::new(Mutex::new(Vec::new()));
        push_event(
            &events,
            "system",
            "startup.preflight_failed",
            json!({ "workspace": workspace_name, "sandbox": sandbox_name, "error": error.to_string() }),
        )
        .await;
        state.sessions.lock().await.insert(
            workspace_name.to_string(),
            AcpEntry::Failed(AcpFailure {
                workspace_name: workspace_name.to_string(),
                initialized: false,
                phase: "preflight".to_string(),
                last_error: error.to_string(),
                last_attempt_ms: now_ms(),
                events,
                pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            }),
        );
        return status(state, workspace_name, 0).await;
    }

    let mut session = start_acp_process(workspace_name, sandbox_name, sandbox).await?;
    let started = async {
        set_phase(&mut session, "initialize").await;
        let init = call_session_with_timeout(
            &mut session,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": client_capabilities(),
                "clientInfo": { "name": "agent-mom", "version": env!("CARGO_PKG_VERSION") },
            }),
            Duration::from_secs(25),
        )
        .await?;
        session.initialized = true;
        push_event(&session.events, "in", "rpc.result.initialize", init.clone()).await;

        set_phase(&mut session, "session/new").await;
        let new_session = call_session_with_timeout(
            &mut session,
            "session/new",
            json!({ "cwd": "/workspace", "mcpServers": [] }),
            Duration::from_secs(25),
        )
        .await?;
        session.session_id = extract_session_id(&new_session);
        push_event(&session.events, "in", "rpc.result.session/new", new_session).await;
        set_phase(&mut session, "ready").await;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = started {
        let phase = session.phase.clone();
        let initialized = session.initialized;
        let events = session.events.clone();
        let pending_permissions = session.pending_permissions.clone();
        let log_tail = read_tail(&session.ssh_log_path, 8192);
        let mut last_error = error.to_string();
        if let Some(log_tail) = log_tail.filter(|value| !value.trim().is_empty()) {
            last_error = format!("{last_error}\n\nstderr:\n{log_tail}");
        }
        push_event(
            &events,
            "system",
            "startup.failed",
            json!({ "phase": phase, "error": last_error }),
        )
        .await;
        cleanup_session(session).await;
        state.sessions.lock().await.insert(
            workspace_name.to_string(),
            AcpEntry::Failed(AcpFailure {
                workspace_name: workspace_name.to_string(),
                initialized,
                phase,
                last_error,
                last_attempt_ms: now_ms(),
                events,
                pending_permissions,
            }),
        );
        return status(state, workspace_name, 0).await;
    }

    state
        .sessions
        .lock()
        .await
        .insert(workspace_name.to_string(), AcpEntry::Running(session));
    status(state, workspace_name, 0).await
}

pub(crate) async fn send_prompt(
    state: &HermesAcpState,
    workspace_name: &str,
    prompt: &str,
    content: &[Value],
) -> Result<AcpStatusResponse> {
    let params = {
        let mut sessions = state.sessions.lock().await;
        let session = running_entry_mut(&mut sessions, workspace_name)?;
        if *session.exited.lock().await {
            bail!("Hermes ACP process exited");
        }
        let message_id = format!("agent-mom-{}", now_ms());
        json!({
            "sessionId": session.session_id,
            "messageId": message_id,
            "prompt": prompt_blocks(prompt, content),
        })
    };
    let result = call(state, workspace_name, "session/prompt", params).await?;
    append_event(state, workspace_name, "rpc.result.session/prompt", result).await;
    status(state, workspace_name, 0).await
}

pub(crate) async fn cancel(
    state: &HermesAcpState,
    workspace_name: &str,
) -> Result<AcpStatusResponse> {
    let cancel_params = {
        let sessions = state.sessions.lock().await;
        let session = running_entry(&sessions, workspace_name)?;
        json!({ "sessionId": session.session_id })
    };
    notify(state, workspace_name, "session/cancel", cancel_params).await?;
    append_event(state, workspace_name, "ui.cancel", json!({})).await;
    status(state, workspace_name, 0).await
}

pub(crate) async fn respond_permission(
    state: &HermesAcpState,
    workspace_name: &str,
    request_id: &str,
    option_id: &str,
) -> Result<AcpStatusResponse> {
    let id = parse_request_id(request_id);
    let (stdin, events, pending_permissions) = {
        let sessions = state.sessions.lock().await;
        let session = running_entry(&sessions, workspace_name)?;
        (
            session.stdin.clone(),
            session.events.clone(),
            session.pending_permissions.clone(),
        )
    };
    pending_permissions.lock().await.remove(request_id);
    respond_raw(&stdin, &events, id, permission_result(option_id)).await?;
    status(state, workspace_name, 0).await
}

pub(crate) async fn events(
    state: &HermesAcpState,
    workspace_name: &str,
    after: u64,
) -> Result<AcpStatusResponse> {
    status(state, workspace_name, after).await
}

#[allow(dead_code)]
pub(crate) fn fake_status(workspace_name: &str, after: u64) -> AcpStatusResponse {
    let event = AcpEvent {
        seq: 1,
        at_ms: now_ms(),
        direction: "system".to_string(),
        kind: "process.started".to_string(),
        payload: json!({ "workspace": workspace_name, "fake": true }),
    };
    AcpStatusResponse {
        workspace: workspace_name.to_string(),
        session_id: Some(format!("fake-{workspace_name}")),
        initialized: true,
        state: "ready".to_string(),
        phase: "ready".to_string(),
        error: None,
        events: if after < 1 { vec![event] } else { Vec::new() },
        pending_permissions: Vec::new(),
        capabilities: Some(json!({ "fake": true })),
    }
}

async fn start_acp_process(
    workspace_name: &str,
    sandbox_name: &str,
    sandbox: &Sandbox,
) -> Result<AcpSession> {
    let key_dir = env::temp_dir().join(format!(
        "mom-hermes-acp-{workspace_name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&key_dir);
    let private_key = key_dir.join("id_ed25519");
    let public_key = key_dir.join("id_ed25519.pub");
    std::fs::create_dir_all(&key_dir).with_context(|| format!("create {}", key_dir.display()))?;
    let keygen = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&private_key)
        .stdin(Stdio::null())
        .output()
        .await
        .context("generate Hermes ACP SSH key")?;
    if !keygen.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&keygen.stderr)
        );
    }
    let public_key_raw = std::fs::read_to_string(&public_key)
        .with_context(|| format!("read {}", public_key.display()))?;
    let authorized_key = public_key_raw
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("parse {}", public_key.display()))?
        .to_string();

    let ssh_server = sandbox
        .ssh()
        .prepare_server_with(|opts| opts.authorized_key(authorized_key).sftp(false))
        .await
        .context("prepare microsandbox Hermes ACP SSH server")?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("bind local microsandbox Hermes ACP SSH server")?;
    let ssh_port = listener
        .local_addr()
        .context("read Hermes ACP listener address")?
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
    let command = "cd /workspace && if command -v hermes-acp >/dev/null 2>&1; then exec hermes-acp; elif command -v hermes >/dev/null 2>&1; then exec hermes acp; else echo 'hermes-acp/hermes acp is not installed' >&2; exit 127; fi";
    let mut child = Command::new("ssh")
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
            "-p",
            &ssh_port.to_string(),
            "root@127.0.0.1",
            command,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(ssh_stderr)
        .spawn()
        .context("start Hermes ACP process")?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Hermes ACP stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Hermes ACP stdout unavailable"))?;
    let stdin = Arc::new(Mutex::new(stdin));
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
    let exited = Arc::new(Mutex::new(false));
    let reader_task = tokio::spawn(read_stdout(
        stdout,
        stdin.clone(),
        Some(sandbox.clone()),
        pending.clone(),
        events.clone(),
        pending_permissions.clone(),
        exited.clone(),
    ));

    let session = AcpSession {
        workspace_name: workspace_name.to_string(),
        session_id: None,
        initialized: false,
        phase: "spawned".to_string(),
        last_error: None,
        next_id: 0,
        stdin,
        pending,
        events,
        pending_permissions,
        exited,
        _sandbox: sandbox.clone(),
        ssh_child: child,
        server_task,
        reader_task,
        key_dir,
        ssh_log_path,
    };
    push_event(
        &session.events,
        "system",
        "process.started",
        json!({ "workspace": workspace_name, "sandbox": sandbox_name }),
    )
    .await;
    Ok(session)
}

async fn status(
    state: &HermesAcpState,
    workspace_name: &str,
    after: u64,
) -> Result<AcpStatusResponse> {
    let sessions = state.sessions.lock().await;
    let entry = sessions
        .get(workspace_name)
        .ok_or_else(|| anyhow!("Hermes ACP session is not running"))?;
    match entry {
        AcpEntry::Running(session) => {
            let events = events_after(&session.events, after).await;
            let pending_permissions = pending_permissions(&session.pending_permissions).await;
            let capabilities = capabilities_from_events(&session.events).await;
            let exited = *session.exited.lock().await;
            Ok(AcpStatusResponse {
                workspace: session.workspace_name.clone(),
                session_id: session.session_id.clone(),
                initialized: session.initialized,
                state: if exited {
                    "exited".to_string()
                } else if session.session_id.is_some() {
                    "ready".to_string()
                } else {
                    "starting".to_string()
                },
                phase: session.phase.clone(),
                error: session.last_error.clone(),
                events,
                pending_permissions,
                capabilities,
            })
        }
        AcpEntry::Failed(failure) => {
            let events = events_after(&failure.events, after).await;
            let pending_permissions = pending_permissions(&failure.pending_permissions).await;
            let capabilities = capabilities_from_events(&failure.events).await;
            Ok(AcpStatusResponse {
                workspace: failure.workspace_name.clone(),
                session_id: None,
                initialized: failure.initialized,
                state: "failed".to_string(),
                phase: failure.phase.clone(),
                error: Some(failure.last_error.clone()),
                events,
                pending_permissions,
                capabilities,
            })
        }
    }
}

fn running_entry<'a>(
    sessions: &'a HashMap<String, AcpEntry>,
    workspace_name: &str,
) -> Result<&'a AcpSession> {
    match sessions.get(workspace_name) {
        Some(AcpEntry::Running(session)) => Ok(session),
        Some(AcpEntry::Failed(failure)) => Err(anyhow!(
            "Hermes ACP failed during {}: {}",
            failure.phase,
            failure.last_error
        )),
        None => Err(anyhow!("Hermes ACP session is not running")),
    }
}

fn running_entry_mut<'a>(
    sessions: &'a mut HashMap<String, AcpEntry>,
    workspace_name: &str,
) -> Result<&'a mut AcpSession> {
    match sessions.get_mut(workspace_name) {
        Some(AcpEntry::Running(session)) => Ok(session),
        Some(AcpEntry::Failed(failure)) => Err(anyhow!(
            "Hermes ACP failed during {}: {}",
            failure.phase,
            failure.last_error
        )),
        None => Err(anyhow!("Hermes ACP session is not running")),
    }
}

async fn events_after(events: &Arc<Mutex<Vec<AcpEvent>>>, after: u64) -> Vec<AcpEvent> {
    events
        .lock()
        .await
        .iter()
        .filter(|event| event.seq > after)
        .cloned()
        .collect()
}

async fn pending_permissions(
    pending_permissions: &Arc<Mutex<HashMap<String, PendingPermission>>>,
) -> Vec<PendingPermission> {
    pending_permissions.lock().await.values().cloned().collect()
}

async fn capabilities_from_events(events: &Arc<Mutex<Vec<AcpEvent>>>) -> Option<Value> {
    events.lock().await.iter().find_map(|event| {
        if event.kind == "rpc.result.initialize" {
            Some(event.payload.clone())
        } else {
            None
        }
    })
}

async fn call(
    state: &HermesAcpState,
    workspace_name: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let target = {
        let mut sessions = state.sessions.lock().await;
        let session = running_entry_mut(&mut sessions, workspace_name)?;
        session.next_id += 1;
        AcpCallTarget {
            id: JsonRpcId::Number(session.next_id),
            stdin: session.stdin.clone(),
            pending: session.pending.clone(),
            events: session.events.clone(),
        }
    };
    call_target_with_timeout(target, method, params, Duration::from_secs(600)).await
}

async fn call_session_with_timeout(
    session: &mut AcpSession,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    session.next_id += 1;
    let id = JsonRpcId::Number(session.next_id);
    let message =
        json!({ "jsonrpc": "2.0", "id": id.as_value(), "method": method, "params": params });
    let (tx, rx) = oneshot::channel();
    session.pending.lock().await.insert(id.clone(), tx);
    push_event(&session.events, "out", method, message.clone()).await;
    if let Err(error) = write_message(&session.stdin, &message).await {
        session.pending.lock().await.remove(&id);
        return Err(error);
    }
    let response = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            session.pending.lock().await.remove(&id);
            return Err(error).context("Hermes ACP response channel closed");
        }
        Err(error) => {
            session.pending.lock().await.remove(&id);
            return Err(error).context("Hermes ACP request timed out");
        }
    };
    if let Some(error) = response.get("error") {
        bail!("Hermes ACP {method} failed: {error}");
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

async fn call_target_with_timeout(
    target: AcpCallTarget,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let message =
        json!({ "jsonrpc": "2.0", "id": target.id.as_value(), "method": method, "params": params });
    let (tx, rx) = oneshot::channel();
    target.pending.lock().await.insert(target.id.clone(), tx);
    push_event(&target.events, "out", method, message.clone()).await;
    if let Err(error) = write_message(&target.stdin, &message).await {
        target.pending.lock().await.remove(&target.id);
        return Err(error);
    }
    let response = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            target.pending.lock().await.remove(&target.id);
            return Err(error).context("Hermes ACP response channel closed");
        }
        Err(error) => {
            target.pending.lock().await.remove(&target.id);
            return Err(error).context("Hermes ACP request timed out");
        }
    };
    if let Some(error) = response.get("error") {
        bail!("Hermes ACP {method} failed: {error}");
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

async fn notify(
    state: &HermesAcpState,
    workspace_name: &str,
    method: &str,
    params: Value,
) -> Result<()> {
    let (stdin, events) = {
        let sessions = state.sessions.lock().await;
        let session = running_entry(&sessions, workspace_name)?;
        (session.stdin.clone(), session.events.clone())
    };
    let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    push_event(&events, "out", method, message.clone()).await;
    write_message(&stdin, &message).await
}

async fn respond_raw(
    stdin: &Arc<Mutex<ChildStdin>>,
    events: &Arc<Mutex<Vec<AcpEvent>>>,
    id: JsonRpcId,
    result: Value,
) -> Result<()> {
    let message = json!({ "jsonrpc": "2.0", "id": id.as_value(), "result": result });
    push_event(events, "out", "rpc.response", message.clone()).await;
    write_message(stdin, &message).await
}

async fn write_message(stdin: &Arc<Mutex<ChildStdin>>, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut stdin = stdin.lock().await;
    stdin.write_all(&body).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_stdout<R>(
    stdout: R,
    stdin: Arc<Mutex<ChildStdin>>,
    sandbox: Option<Sandbox>,
    pending: Arc<Mutex<HashMap<JsonRpcId, oneshot::Sender<Value>>>>,
    events: Arc<Mutex<Vec<AcpEvent>>>,
    pending_permissions: Arc<Mutex<HashMap<String, PendingPermission>>>,
    exited: Arc<Mutex<bool>>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stdout);
    loop {
        let value = match read_message(&mut reader).await {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(error) => {
                push_event(
                    &events,
                    "system",
                    "transport.error",
                    json!({ "error": error.to_string() }),
                )
                .await;
                break;
            }
        };
        let kind = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| value.get("id").map(|_| "rpc.response".to_string()))
            .unwrap_or_else(|| "rpc.message".to_string());
        push_event(&events, "in", &kind, value.clone()).await;

        if let Some(id) = value.get("id").and_then(JsonRpcId::from_value) {
            if value.get("method").is_some() {
                handle_client_request(
                    id,
                    value,
                    &stdin,
                    sandbox.as_ref(),
                    &events,
                    &pending_permissions,
                )
                .await;
                continue;
            }
            if let Some(tx) = pending.lock().await.remove(&id) {
                if response_ends_turn(&value) {
                    pending_permissions.lock().await.clear();
                }
                let _ = tx.send(value);
            }
        }
    }
    *exited.lock().await = true;
    pending.lock().await.clear();
    push_event(&events, "system", "process.exited", json!({})).await;
}

async fn handle_client_request(
    id: JsonRpcId,
    value: Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    sandbox: Option<&Sandbox>,
    events: &Arc<Mutex<Vec<AcpEvent>>>,
    pending_permissions: &Arc<Mutex<HashMap<String, PendingPermission>>>,
) {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if method.contains("permission") {
        let permission = PendingPermission {
            id: format_request_id(&id),
            method,
            params: value.get("params").cloned().unwrap_or(Value::Null),
        };
        pending_permissions
            .lock()
            .await
            .insert(permission.id.clone(), permission);
        return;
    }

    let result = match method.as_str() {
        "fs/read_text_file" | "fs/readTextFile" => match sandbox {
            Some(sandbox) => handle_fs_read_text_file(sandbox, value.get("params")).await,
            None => Err(anyhow!("fs/read_text_file requires a sandbox")),
        },
        "fs/write_text_file" | "fs/writeTextFile" => match sandbox {
            Some(sandbox) => handle_fs_write_text_file(sandbox, value.get("params")).await,
            None => Err(anyhow!("fs/write_text_file requires a sandbox")),
        },
        _ => Err(anyhow!(
            "Agent Mom does not implement Hermes ACP client request method '{method}'"
        )),
    };

    match result {
        Ok(result) => {
            let response = json!({ "jsonrpc": "2.0", "id": id.as_value(), "result": result });
            push_event(events, "out", "rpc.response", response.clone()).await;
            let _ = write_message(stdin, &response).await;
        }
        Err(error) => {
            let response = json!({
                "jsonrpc": "2.0",
                "id": id.as_value(),
                "error": { "code": -32000, "message": error.to_string() }
            });
            push_event(events, "out", "rpc.error_response", response.clone()).await;
            let _ = write_message(stdin, &response).await;
        }
    }
}

fn client_capabilities() -> Value {
    json!({
        "_meta": {
            "terminal-auth": true
        },
        "fs": {
            "readTextFile": true,
            "writeTextFile": true
        },
        "terminal": false
    })
}

fn prompt_blocks(prompt: &str, content: &[Value]) -> Vec<Value> {
    let mut blocks = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str).is_some())
        .cloned()
        .collect::<Vec<_>>();
    if blocks.is_empty() && !prompt.trim().is_empty() {
        blocks.push(json!({ "type": "text", "text": prompt }));
    }
    blocks
}

async fn set_phase(session: &mut AcpSession, phase: &str) {
    session.phase = phase.to_string();
    push_event(
        &session.events,
        "system",
        "startup.phase",
        json!({ "phase": phase }),
    )
    .await;
}

async fn preflight_hermes(sandbox: &Sandbox) -> Result<()> {
    let output = sandbox
        .shell(
            r#"
set -eu
if command -v hermes-acp >/dev/null 2>&1; then
  hermes-acp --check >/tmp/mom-acp-preflight-hermes.out 2>/tmp/mom-acp-preflight-hermes.err || {
    cat /tmp/mom-acp-preflight-hermes.err >&2
    exit 1
  }
elif command -v hermes >/dev/null 2>&1; then
  hermes acp --check >/tmp/mom-acp-preflight-hermes.out 2>/tmp/mom-acp-preflight-hermes.err || {
    cat /tmp/mom-acp-preflight-hermes.err >&2
    exit 1
  }
else
  echo 'hermes-acp/hermes is not installed or not on PATH' >&2
  exit 127
fi
"#,
        )
        .await
        .context("run Hermes ACP preflight")?;
    if !output.status().success {
        let stderr = output
            .stderr()
            .unwrap_or_else(|_| "Hermes ACP preflight failed".to_string());
        bail!("{}", stderr.trim());
    }
    Ok(())
}

fn parse_request_id(request_id: &str) -> JsonRpcId {
    request_id
        .parse::<u64>()
        .map(JsonRpcId::Number)
        .unwrap_or_else(|_| JsonRpcId::String(request_id.to_string()))
}

fn format_request_id(id: &JsonRpcId) -> String {
    match id {
        JsonRpcId::Number(id) => id.to_string(),
        JsonRpcId::String(id) => id.clone(),
    }
}

fn response_ends_turn(value: &Value) -> bool {
    value
        .get("result")
        .and_then(|result| {
            result
                .get("stopReason")
                .or_else(|| result.get("stop_reason"))
        })
        .and_then(Value::as_str)
        .is_some()
}

fn permission_result(option_id: &str) -> Value {
    let denied = matches!(
        option_id,
        "deny" | "deny_always" | "reject" | "cancel" | "cancelled" | "canceled"
    );
    if denied {
        json!({ "outcome": { "outcome": "cancelled" } })
    } else {
        json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
                "option_id": option_id
            }
        })
    }
}

async fn handle_fs_read_text_file(sandbox: &Sandbox, params: Option<&Value>) -> Result<Value> {
    let params = params.ok_or_else(|| anyhow!("fs/read_text_file missing params"))?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("fs/read_text_file missing path"))?;
    validate_workspace_path(path)?;
    let line = optional_positive_usize(params, "line")?;
    let limit = optional_positive_usize(params, "limit")?;
    let request = json!({ "op": "read", "path": path, "line": line, "limit": limit });
    let output = run_fs_helper(sandbox, request).await?;
    Ok(json!({ "content": output }))
}

async fn handle_fs_write_text_file(sandbox: &Sandbox, params: Option<&Value>) -> Result<Value> {
    let params = params.ok_or_else(|| anyhow!("fs/write_text_file missing params"))?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("fs/write_text_file missing path"))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("fs/write_text_file missing content"))?;
    validate_workspace_path(path)?;
    let request = json!({ "op": "write", "path": path, "content": content });
    run_fs_helper(sandbox, request).await?;
    Ok(json!({}))
}

fn optional_positive_usize(params: &Value, key: &str) -> Result<Option<usize>> {
    params
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| anyhow!("{key} must be a positive integer"))
                .and_then(|value| {
                    usize::try_from(value).with_context(|| format!("{key} is too large"))
                })
        })
        .transpose()
}

fn validate_workspace_path(path: &str) -> Result<()> {
    if path.contains('\0') {
        bail!("path contains NUL byte");
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("path must be absolute");
    }
    if path != Path::new("/workspace") && !path.starts_with("/workspace/") {
        bail!("path must be inside /workspace");
    }
    for component in path.components() {
        let value = component.as_os_str().to_string_lossy();
        if value == ".." {
            bail!("path may not contain '..'");
        }
    }
    let protected = [
        ".env",
        ".ssh",
        ".codex",
        ".hermes",
        ".hermes-agent",
        "auth.json",
    ];
    let path_string = path.to_string_lossy();
    for segment in protected {
        if path_string == format!("/workspace/{segment}")
            || path_string.contains(&format!("/{segment}/"))
            || path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                segment == ".env" && (name == ".env" || name.starts_with(".env."))
            })
        {
            bail!("path is protected");
        }
    }
    Ok(())
}

async fn run_fs_helper(sandbox: &Sandbox, request: Value) -> Result<String> {
    let request = crate::shell_quote(&serde_json::to_string(&request)?);
    let script = format!(
        r#"ACP_FS_REQUEST={request} python3 - <<'PY'
import json
import os
from pathlib import Path

req = json.loads(os.environ["ACP_FS_REQUEST"])
path = Path(req["path"])
workspace = Path("/workspace").resolve()

if req["op"] == "write":
    parent = path.parent.resolve(strict=True)
    if parent != workspace and workspace not in parent.parents:
        raise SystemExit("path escapes /workspace")
    if path.exists() or path.is_symlink():
        resolved = path.resolve(strict=True)
        if resolved != workspace and workspace not in resolved.parents:
            raise SystemExit("path escapes /workspace")
    content = req.get("content", "")
    if len(content.encode("utf-8")) > 1024 * 1024:
        raise SystemExit("content is too large")
    path.write_text(content, encoding="utf-8")
    print("")
elif req["op"] == "read":
    resolved = path.resolve(strict=True)
    if resolved != workspace and workspace not in resolved.parents:
        raise SystemExit("path escapes /workspace")
    if not resolved.is_file():
        raise SystemExit("path is not a regular file")
    data = resolved.read_bytes()
    if len(data) > 1024 * 1024:
        raise SystemExit("file is too large")
    content = data.decode("utf-8")
    line = req.get("line")
    limit = req.get("limit")
    if line:
        lines = content.splitlines()
        start = max(int(line) - 1, 0)
        end = start + int(limit or 200)
        content = "\n".join(lines[start:end])
    elif limit:
        content = content[: int(limit)]
    print(content, end="")
else:
    raise SystemExit("unknown op")
PY"#,
    );
    let output = sandbox.shell(&script).await.context("run ACP fs helper")?;
    let stdout = output.stdout().unwrap_or_else(|_| String::new());
    if !output.status().success {
        let stderr = output
            .stderr()
            .unwrap_or_else(|_| "ACP fs helper failed".to_string());
        bail!("{}", stderr.trim());
    }
    Ok(stdout)
}

fn extract_session_id(value: &Value) -> Option<String> {
    value
        .get("sessionId")
        .or_else(|| value.get("session_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            value
                .get("session")
                .and_then(|session| {
                    session
                        .get("sessionId")
                        .or_else(|| session.get("session_id"))
                })
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

async fn read_message<R>(reader: &mut BufReader<R>) -> Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(None);
    }
    let value = serde_json::from_str(line.trim_end())?;
    Ok(Some(value))
}

async fn push_event(
    events: &Arc<Mutex<Vec<AcpEvent>>>,
    direction: &str,
    kind: &str,
    payload: Value,
) {
    let mut events = events.lock().await;
    let seq = events.last().map(|event| event.seq + 1).unwrap_or(1);
    events.push(AcpEvent {
        seq,
        at_ms: now_ms(),
        direction: direction.to_string(),
        kind: kind.to_string(),
        payload,
    });
    if events.len() > EVENT_CAP {
        let overflow = events.len() - EVENT_CAP;
        events.drain(0..overflow);
    }
}

async fn append_event(state: &HermesAcpState, workspace_name: &str, kind: &str, payload: Value) {
    let events = {
        let sessions = state.sessions.lock().await;
        match sessions.get(workspace_name) {
            Some(AcpEntry::Running(session)) => Some(session.events.clone()),
            Some(AcpEntry::Failed(failure)) => Some(failure.events.clone()),
            None => None,
        }
    };
    if let Some(events) = events {
        push_event(&events, "system", kind, payload).await;
    }
}

async fn cleanup_session(mut session: AcpSession) {
    let _ = session.ssh_child.kill().await;
    session.server_task.abort();
    session.reader_task.abort();
    let _ = std::fs::remove_dir_all(&session.key_dir);
}

fn read_tail(path: &Path, max_bytes: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let start = data.len().saturating_sub(max_bytes);
    Some(String::from_utf8_lossy(&data[start..]).to_string())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
