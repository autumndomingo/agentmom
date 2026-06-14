use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::Mutex,
};

use crate::GuestVm;

#[derive(Clone, Default)]
pub(crate) struct HermesAcpState {
    sessions: Arc<Mutex<HashMap<String, RunningAcp>>>,
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
    socket: WebSocket,
) {
    match start_acp_process(&state, &workspace_name, &vm_name, &vm).await {
        Ok(process) => pipe_socket_to_acp(socket, process).await,
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
) -> Result<AcpProcess> {
    cleanup_workspace(state, workspace_name).await;
    preflight_hermes(vm).await?;
    let config = crate::load_mom_config()?;
    let command = acp_shell_command(&config);
    let mut child = vm
        .spawn_shell(&command)
        .await
        .context("start Hermes ACP process")?;

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
    Ok(AcpProcess { stdin, stdout })
}

fn acp_shell_command(config: &crate::config::MomConfig) -> String {
    let hermes_home = format!("{}/{}", crate::GUEST_HERMES_HOME, config.hermes_profile());
    let hermes_home = crate::shell_quote(&hermes_home);
    format!(
        "stty -echo 2>/dev/null || true; if command -v agentmom-hermes-acp >/dev/null 2>&1; then exec agentmom-hermes-acp; fi; if [ -f /etc/profile.d/agentmom-proxy.sh ]; then . /etc/profile.d/agentmom-proxy.sh; fi; export HERMES_HOME={hermes_home}; cd /workspace && if command -v hermes-acp >/dev/null 2>&1; then exec hermes-acp; elif command -v hermes >/dev/null 2>&1; then exec hermes acp; else echo 'hermes-acp/hermes acp is not installed' >&2; exit 127; fi"
    )
}

async fn pipe_socket_to_acp(mut socket: WebSocket, process: AcpProcess) {
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

    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut stdin = process.stdin;
    let mut stdout = BufReader::new(process.stdout);
    let pending_echoes = Arc::new(Mutex::new(VecDeque::<String>::new()));
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
            if ws_tx.send(Message::Text(message.into())).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let pending_echoes = pending_echoes.clone();
    let ws_to_stdin = async {
        while let Some(Ok(message)) = ws_rx.next().await {
            match message {
                Message::Text(text) => {
                    pending_echoes.lock().await.push_back(text.to_string());
                    stdin.write_all(text.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Message::Binary(bytes) => {
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
