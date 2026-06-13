use std::{collections::HashMap, env, net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use microsandbox::Sandbox;
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
    task::JoinHandle,
};

#[derive(Clone, Default)]
pub(crate) struct HermesAcpState {
    sessions: Arc<Mutex<HashMap<String, RunningAcp>>>,
}

struct RunningAcp {
    child: Child,
    server_task: JoinHandle<()>,
    key_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerAcpWsQuery {
    pub(crate) workspace_name: String,
    pub(crate) sandbox_name: String,
}

pub(crate) async fn bridge_worker_socket(
    state: HermesAcpState,
    workspace_name: String,
    sandbox_name: String,
    sandbox: Sandbox,
    socket: WebSocket,
) {
    match start_acp_process(&state, &workspace_name, &sandbox_name, &sandbox).await {
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
    sandbox_name: &str,
    sandbox: &Sandbox,
) -> Result<AcpProcess> {
    cleanup_workspace(state, workspace_name).await;
    preflight_hermes(sandbox).await?;
    let config = crate::load_mom_config()?;
    let command = acp_shell_command(&config);

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
        ])
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

    state.sessions.lock().await.insert(
        workspace_name.to_string(),
        RunningAcp {
            child,
            server_task,
            key_dir,
        },
    );

    let _ = sandbox_name;
    Ok(AcpProcess { stdin, stdout })
}

fn acp_shell_command(config: &crate::config::MomConfig) -> String {
    let hermes_home = format!("{}/{}", crate::GUEST_HERMES_HOME, config.hermes_profile());
    let hermes_home = crate::shell_quote(&hermes_home);
    format!(
        "if [ -f /etc/profile.d/agentmom-proxy.sh ]; then . /etc/profile.d/agentmom-proxy.sh; fi; export HERMES_HOME={hermes_home}; cd /workspace && if command -v hermes-acp >/dev/null 2>&1; then exec hermes-acp; elif command -v hermes >/dev/null 2>&1; then exec hermes acp; else echo 'hermes-acp/hermes acp is not installed' >&2; exit 127; fi"
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
    let stdout_to_ws = async {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = stdout.read_line(&mut line).await?;
            if bytes == 0 {
                break;
            }
            let message = line.trim_end_matches(['\r', '\n']).to_string();
            if !serde_json::from_str::<serde_json::Value>(&message)
                .is_ok_and(|value| value.is_object())
            {
                eprintln!(
                    "dropping non-JSON Hermes ACP stdout line ({} bytes)",
                    message.len()
                );
                continue;
            }
            if ws_tx.send(Message::Text(message.into())).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let ws_to_stdin = async {
        while let Some(Ok(message)) = ws_rx.next().await {
            match message {
                Message::Text(text) => {
                    stdin.write_all(text.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Message::Binary(bytes) => {
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
        session.server_task.abort();
        let _ = std::fs::remove_dir_all(session.key_dir);
    }
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
