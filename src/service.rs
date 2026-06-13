use std::{collections::HashMap, env, net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use microsandbox::{Sandbox, sandbox::SandboxStatus};
use tokio::{process::Command, sync::Mutex, task::JoinHandle};

use crate::{HERMES_GUEST_PORT, OPENCODE_GUEST_PORT, checked_shell, shell_quote};

struct ServiceTunnel {
    url: String,
    health_url: String,
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

#[derive(Clone, Default)]
pub(crate) struct ServiceState {
    opencode_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
    hermes_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
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

pub(crate) async fn open_workspace_service(
    state: &ServiceState,
    workspace_name: &str,
    sandbox_name: &str,
    service_id: &str,
) -> Result<String> {
    match service_id {
        "opencode" => {
            ensure_guest_service_tunnel(
                workspace_name,
                sandbox_name,
                &OPENCODE_SERVICE,
                &state.opencode_tunnels,
            )
            .await
        }
        "hermes" => {
            ensure_guest_service_tunnel(
                workspace_name,
                sandbox_name,
                &HERMES_SERVICE,
                &state.hermes_tunnels,
            )
            .await
        }
        other => bail!("unknown workspace service: {other}"),
    }
}

async fn ensure_guest_service_tunnel(
    workspace_name: &str,
    sandbox_name: &str,
    service: &GuestServiceSpec,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    {
        let mut active = tunnels.lock().await;
        if let Some(tunnel) = active.get_mut(workspace_name) {
            if tunnel_is_healthy(&tunnel.health_url, service.health_path).await {
                return Ok(tunnel.url.clone());
            }
            let _ = tunnel.ssh_child.kill().await;
            tunnel.server_task.abort();
            let _ = std::fs::remove_dir_all(&tunnel.key_dir);
            active.remove(workspace_name);
        }
    }

    let host_port = reserve_host_port().await?;
    let tunnel_bind_host = service_tunnel_bind_host();
    let health_url = format!("http://127.0.0.1:{host_port}");
    let public_url = service_tunnel_public_url(&tunnel_bind_host, host_port);
    let sandbox = running_sandbox_owned(sandbox_name).await?;
    ensure_guest_service(&sandbox, service).await?;
    let tunnel = start_service_tunnel(
        workspace_name,
        service,
        &sandbox,
        &tunnel_bind_host,
        host_port,
    )
    .await?;
    wait_for_tunnel(
        workspace_name,
        tunnel,
        &health_url,
        &public_url,
        service,
        tunnels,
    )
    .await
}

async fn start_service_tunnel(
    workspace_name: &str,
    service: &GuestServiceSpec,
    sandbox: &Sandbox,
    tunnel_bind_host: &str,
    host_port: u16,
) -> Result<ServiceTunnel> {
    let key_dir = env::temp_dir().join(format!(
        "mom-{}-{}-{}",
        service.id,
        workspace_name,
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
            &format!(
                "{tunnel_bind_host}:{host_port}:127.0.0.1:{}",
                service.guest_port
            ),
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
        url: service_tunnel_public_url(tunnel_bind_host, host_port),
        health_url: format!("http://127.0.0.1:{host_port}"),
        _sandbox: sandbox.clone(),
        ssh_child,
        server_task,
        key_dir,
    })
}

async fn wait_for_tunnel(
    workspace_name: &str,
    mut tunnel: ServiceTunnel,
    health_url: &str,
    public_url: &str,
    service: &GuestServiceSpec,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    for _ in 0..50 {
        if tunnel_is_healthy(health_url, service.health_path).await {
            tunnels
                .lock()
                .await
                .insert(workspace_name.to_string(), tunnel);
            return Ok(public_url.to_string());
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
    bail!(
        "{} tunnel did not become reachable at {public_url}; {ssh_status}\n{ssh_log}",
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

async fn reserve_host_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .context("reserve local service tunnel port")?;
    let port = listener
        .local_addr()
        .context("read reserved service tunnel port")?
        .port();
    drop(listener);
    Ok(port)
}

fn service_tunnel_bind_host() -> String {
    env::var("MOM_SERVICE_TUNNEL_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn service_tunnel_public_url(bind_host: &str, port: u16) -> String {
    if let Ok(base) = env::var("MOM_SERVICE_TUNNEL_BASE_URL") {
        return service_tunnel_public_url_from_base(bind_host, port, Some(&base));
    }
    service_tunnel_public_url_from_base(bind_host, port, None)
}

fn service_tunnel_public_url_from_base(bind_host: &str, port: u16, base: Option<&str>) -> String {
    if let Some(base) = base {
        if base.contains("{port}") {
            return base.replace("{port}", &port.to_string());
        }
        return format!("{}:{port}", base.trim_end_matches('/'));
    }
    let host = if bind_host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind_host
    };
    format!("http://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::service_tunnel_public_url_from_base;

    #[test]
    fn service_tunnel_url_supports_port_template() {
        let url = service_tunnel_public_url_from_base(
            "0.0.0.0",
            45887,
            Some("https://mom-1-{port}.agentmom.xyz/"),
        );
        assert_eq!(url, "https://mom-1-45887.agentmom.xyz/");
    }

    #[test]
    fn service_tunnel_url_keeps_legacy_base_port_behavior() {
        let url =
            service_tunnel_public_url_from_base("0.0.0.0", 45887, Some("http://100.81.250.67"));
        assert_eq!(url, "http://100.81.250.67:45887");
    }
}
