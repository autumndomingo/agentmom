use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::Mutex,
};

use crate::GuestVm;

#[derive(Debug, Clone, Default)]
pub(crate) struct AcpBridgeTiming {
    pub(crate) worker_vm_status: Option<String>,
    pub(crate) worker_ensure_vm_ms: Option<u128>,
    pub(crate) worker_vm_connect_ssh_ms: Option<u128>,
    pub(crate) worker_vm_refresh_definition_ms: Option<u128>,
    pub(crate) worker_vm_systemctl_start_ms: Option<u128>,
    pub(crate) worker_vm_systemd_active_ms: Option<u128>,
    pub(crate) worker_vm_ch_api_ms: Option<u128>,
    pub(crate) worker_vm_net_prime_ms: Option<u128>,
    pub(crate) worker_vm_ch_resume_ms: Option<u128>,
    pub(crate) worker_vm_tcp_22_ready_ms: Option<u128>,
    pub(crate) worker_vm_ssh_ready_ms: Option<u128>,
    pub(crate) worker_api_update_workspace_ms: Option<u128>,
}

#[derive(Clone, Default)]
pub(crate) struct HermesAcpState {
    sessions: Arc<Mutex<HashMap<String, RunningAcp>>>,
    preflight_ok: Arc<Mutex<HashSet<String>>>,
}

struct RunningAcp {
    child: Child,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerAcpWsQuery {
    pub(crate) workspace_name: String,
    pub(crate) vm_name: String,
}

pub(crate) async fn bridge_worker_socket(
    state: HermesAcpState,
    workspace_name: String,
    vm_name: String,
    vm: GuestVm,
    mut socket: WebSocket,
    bridge_timing: AcpBridgeTiming,
) {
    let bridge_started = Instant::now();
    let _ = send_timing(&mut socket, "worker_vm_ready", &bridge_timing).await;
    match start_acp_process(&state, &workspace_name, &vm_name, &vm).await {
        Ok((process, startup_timing)) => {
            let _ = send_startup_timing(&mut socket, &startup_timing, bridge_started).await;
            pipe_socket_to_acp(socket, process, vm).await;
        }
        Err(error) => {
            let _ = send_status(socket, "error", &format!("{error:#}")).await;
        }
    }
    cleanup_workspace(&state, &workspace_name).await;
}

pub(crate) async fn fake_worker_socket(workspace_name: String, mut socket: WebSocket) {
    let _ = socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/status",
                "params": { "state": "connected", "workspace": workspace_name, "fake": true }
            })
            .to_string()
            .into(),
        ))
        .await;
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => {
                let _ = socket.send(Message::Text(text)).await;
            }
            Message::Binary(bytes) => {
                let _ = socket.send(Message::Binary(bytes)).await;
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

struct AcpProcess {
    stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
}

async fn start_acp_process(
    state: &HermesAcpState,
    workspace_name: &str,
    vm_name: &str,
    vm: &GuestVm,
) -> Result<(AcpProcess, AcpStartupTiming)> {
    let started = Instant::now();
    cleanup_workspace(state, workspace_name).await;
    let cleanup_ms = started.elapsed().as_millis();
    let preflight_started = Instant::now();
    let preflight_cached = state.preflight_ok.lock().await.contains(workspace_name);
    if !preflight_cached {
        preflight_hermes(vm).await?;
        state
            .preflight_ok
            .lock()
            .await
            .insert(workspace_name.to_string());
    }
    let preflight_ms = preflight_started.elapsed().as_millis();
    let config_started = Instant::now();
    let config = crate::load_mom_config()?;
    let command = acp_shell_command(&config);
    let config_ms = config_started.elapsed().as_millis();
    let spawn_started = Instant::now();
    let mut child = vm
        .spawn_shell_ready(&command)
        .await
        .context("start Hermes ACP process")?;
    let spawn_shell_ms = spawn_started.elapsed().as_millis();

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Hermes ACP stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Hermes ACP stdout unavailable"))?;

    state
        .sessions
        .lock()
        .await
        .insert(workspace_name.to_string(), RunningAcp { child });

    let _ = vm_name;
    Ok((
        AcpProcess { stdin, stdout },
        AcpStartupTiming {
            cleanup_ms,
            preflight_ms,
            preflight_cached,
            config_ms,
            spawn_shell_ms,
            total_ms: started.elapsed().as_millis(),
        },
    ))
}

#[derive(Debug, Clone)]
struct AcpStartupTiming {
    cleanup_ms: u128,
    preflight_ms: u128,
    preflight_cached: bool,
    config_ms: u128,
    spawn_shell_ms: u128,
    total_ms: u128,
}

fn acp_shell_command(config: &crate::config::MomConfig) -> String {
    let hermes_home = format!("{}/{}", crate::GUEST_HERMES_HOME, config.hermes_profile());
    let hermes_home = crate::shell_quote(&hermes_home);
    format!(
        "stty -echo 2>/dev/null || true; if command -v agentmom-hermes-acp >/dev/null 2>&1; then exec agentmom-hermes-acp; fi; if [ -f /etc/profile.d/agentmom-proxy.sh ]; then . /etc/profile.d/agentmom-proxy.sh; fi; export HERMES_HOME={hermes_home}; cd /workspace && if command -v hermes-acp >/dev/null 2>&1; then exec hermes-acp; elif command -v hermes >/dev/null 2>&1; then exec hermes acp; else echo 'hermes-acp/hermes acp is not installed' >&2; exit 127; fi"
    )
}

async fn pipe_socket_to_acp(mut socket: WebSocket, process: AcpProcess, vm: GuestVm) {
    let _ = socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/status",
                "params": { "state": "connected" }
            })
            .to_string()
            .into(),
        ))
        .await;

    let (ws_tx, mut ws_rx) = socket.split();
    let ws_tx = Arc::new(Mutex::new(ws_tx));
    let mut stdin = process.stdin;
    let mut stdout = BufReader::new(process.stdout);
    let pending_echoes = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let ws_stdout_tx = ws_tx.clone();
    let stdout_to_ws = async {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = stdout.read_line(&mut line).await?;
            if bytes == 0 {
                break;
            }
            let message = line.trim_end_matches(['\r', '\n']).to_string();
            let mut echoes = pending_echoes.lock().await;
            if let Some(index) = echoes.iter().position(|echo| echo == &message) {
                echoes.remove(index);
                continue;
            }
            drop(echoes);

            if !serde_json::from_str::<serde_json::Value>(&message)
                .is_ok_and(|value| value.is_object())
            {
                let preview: String = message.chars().take(500).collect();
                eprintln!(
                    "dropping non-JSON Hermes ACP stdout line ({} bytes): {}",
                    message.len(),
                    preview
                );
                continue;
            }
            if ws_stdout_tx
                .lock()
                .await
                .send(Message::Text(message.into()))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let pending_echoes = pending_echoes.clone();
    let ws_stdin_tx = ws_tx.clone();
    let ws_to_stdin = async {
        while let Some(Ok(message)) = ws_rx.next().await {
            match message {
                Message::Text(text) => {
                    resume_for_inbound_message(&vm, &ws_stdin_tx).await?;
                    if handle_test_guest_ping(&vm, &ws_stdin_tx, &text).await? {
                        continue;
                    }
                    pending_echoes.lock().await.push_back(text.to_string());
                    stdin.write_all(text.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Message::Binary(bytes) => {
                    resume_for_inbound_message(&vm, &ws_stdin_tx).await?;
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        pending_echoes.lock().await.push_back(text.to_string());
                    }
                    stdin.write_all(&bytes).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        _ = stdout_to_ws => {},
        _ = ws_to_stdin => {},
    }
}

async fn handle_test_guest_ping(
    vm: &GuestVm,
    ws_tx: &Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
    text: &str,
) -> Result<bool> {
    let Ok(request) = serde_json::from_str::<Value>(text) else {
        return Ok(false);
    };
    if request.get("method").and_then(Value::as_str) != Some("mom/test/guest-ping") {
        return Ok(false);
    }
    let Some(id) = request.get("id").cloned() else {
        return Ok(true);
    };

    let response = if test_endpoints_enabled() {
        match vm.guest_ping().await {
            Ok((stdout, guest_ping_ms)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "ok": true,
                    "vm": vm.name(),
                    "stdout": stdout,
                    "guest_ping_ms": guest_ping_ms,
                }
            }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": format!("guest ping failed: {error:#}"),
                }
            }),
        }
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": "test endpoints are disabled",
            }
        })
    };

    ws_tx
        .lock()
        .await
        .send(Message::Text(response.to_string().into()))
        .await
        .map_err(|error| anyhow!("send guest ping response: {error}"))?;
    Ok(true)
}

fn test_endpoints_enabled() -> bool {
    std::env::var("MOM_ENABLE_TEST_ENDPOINTS").is_ok_and(|value| value == "1")
}

async fn resume_for_inbound_message(
    vm: &GuestVm,
    ws_tx: &Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
) -> Result<()> {
    let started = Instant::now();
    let resumed = vm.resume_if_state_file_paused_fast().await?;
    if !resumed {
        return Ok(());
    }
    ws_tx
        .lock()
        .await
        .send(timing_message(
            "acp_inbound_resume",
            json!({
                "resumed": resumed,
                "acp_inbound_resume_ms": started.elapsed().as_millis(),
            }),
        ))
        .await
        .map_err(|error| anyhow!("send websocket inbound resume timing: {error}"))?;
    Ok(())
}

async fn send_status(mut socket: WebSocket, state: &str, message: &str) -> Result<()> {
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/status",
                "params": { "state": state, "message": message }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| anyhow!("send websocket status: {error}"))
}

async fn send_timing(
    socket: &mut WebSocket,
    phase: &str,
    bridge_timing: &AcpBridgeTiming,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/timing",
                "params": {
                    "phase": phase,
                    "worker_vm_status": bridge_timing.worker_vm_status,
                    "worker_ensure_vm_ms": bridge_timing.worker_ensure_vm_ms,
                    "worker_vm_connect_ssh_ms": bridge_timing.worker_vm_connect_ssh_ms,
                    "worker_vm_refresh_definition_ms": bridge_timing.worker_vm_refresh_definition_ms,
                    "worker_vm_systemctl_start_ms": bridge_timing.worker_vm_systemctl_start_ms,
                    "worker_vm_systemd_active_ms": bridge_timing.worker_vm_systemd_active_ms,
                    "worker_vm_ch_api_ms": bridge_timing.worker_vm_ch_api_ms,
                    "worker_vm_net_prime_ms": bridge_timing.worker_vm_net_prime_ms,
                    "worker_vm_ch_resume_ms": bridge_timing.worker_vm_ch_resume_ms,
                    "worker_vm_tcp_22_ready_ms": bridge_timing.worker_vm_tcp_22_ready_ms,
                    "worker_vm_ssh_ready_ms": bridge_timing.worker_vm_ssh_ready_ms,
                    "worker_api_update_workspace_ms": bridge_timing.worker_api_update_workspace_ms,
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| anyhow!("send websocket timing: {error}"))
}

async fn send_startup_timing(
    socket: &mut WebSocket,
    timing: &AcpStartupTiming,
    bridge_started: Instant,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "mom/timing",
                "params": {
                    "phase": "acp_started",
                    "acp_cleanup_ms": timing.cleanup_ms,
                    "acp_preflight_ms": timing.preflight_ms,
                    "acp_preflight_cached": timing.preflight_cached,
                    "acp_config_ms": timing.config_ms,
                    "acp_spawn_shell_ms": timing.spawn_shell_ms,
                    "acp_startup_ms": timing.total_ms,
                    "worker_upgrade_to_acp_started_ms": bridge_started.elapsed().as_millis(),
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| anyhow!("send websocket startup timing: {error}"))
}

fn timing_message(phase: &str, mut params: serde_json::Value) -> Message {
    if let Some(params) = params.as_object_mut() {
        params.insert(
            "phase".to_string(),
            serde_json::Value::String(phase.to_string()),
        );
    }
    Message::Text(
        json!({
            "jsonrpc": "2.0",
            "method": "mom/timing",
            "params": params,
        })
        .to_string()
        .into(),
    )
}

async fn cleanup_workspace(state: &HermesAcpState, workspace_name: &str) {
    let session = state.sessions.lock().await.remove(workspace_name);
    if let Some(mut session) = session {
        let _ = session.child.kill().await;
    }
}

async fn preflight_hermes(vm: &GuestVm) -> Result<()> {
    let output = vm
        .shell(preflight_hermes_script())
        .await
        .context("run Hermes ACP preflight")?;
    if !output.ok {
        bail!("{}", output.stderr.trim());
    }
    Ok(())
}

fn preflight_hermes_script() -> &'static str {
    r#"
set -e
if command -v agentmom-hermes-acp >/dev/null 2>&1; then
  agentmom-hermes-acp --check >/tmp/mom-acp-preflight-hermes.out 2>/tmp/mom-acp-preflight-hermes.err || {
    cat /tmp/mom-acp-preflight-hermes.err >&2
    exit 1
  }
  exit 0
fi
if [ -f /etc/profile.d/mom.sh ]; then . /etc/profile.d/mom.sh; fi
if [ -f /etc/profile.d/agentmom-proxy.sh ]; then . /etc/profile.d/agentmom-proxy.sh; fi
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
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_preflight_prefers_guest_wrapper_with_profile_fallback() {
        let script = preflight_hermes_script();

        assert!(script.contains("command -v agentmom-hermes-acp"));
        assert!(script.contains("set -e\n"));
        assert!(!script.contains("set -eu"));
        assert!(script.contains("agentmom-hermes-acp --check"));
        assert!(script.contains(". /etc/profile.d/mom.sh"));
        assert!(script.contains(". /etc/profile.d/agentmom-proxy.sh"));
        let wrapper_check = script.find("agentmom-hermes-acp --check").unwrap();
        let raw_check = script.find("\n  hermes-acp --check").unwrap();
        assert!(wrapper_check < raw_check);
    }
}
