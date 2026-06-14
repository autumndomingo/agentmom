use std::{collections::HashMap, env, net::SocketAddr, process::Stdio, sync::Arc};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, sync::Mutex};

use crate::{GuestVm, HERMES_GUEST_PORT, VmStatus, checked_shell, get_vm, shell_quote};

struct ServiceTunnel {
    url: String,
    health_url: String,
    _vm: GuestVm,
    ssh_child: tokio::process::Child,
}

const HERMES_HEALTH_PATH: &str = "/api/status";
const HERMES_WORKDIR: &str = "/workspace";
const HERMES_LOG_PATH: &str = "/tmp/mom-hermes/dashboard.log";
const HERMES_READINESS_ATTEMPTS: u16 = 90;

#[derive(Clone, Default)]
pub(crate) struct ServiceState {
    hermes_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
}

pub(crate) async fn open_hermes_dashboard(
    state: &ServiceState,
    workspace_name: &str,
    vm_name: &str,
) -> Result<String> {
    ensure_hermes_tunnel(workspace_name, vm_name, &state.hermes_tunnels).await
}

async fn ensure_hermes_tunnel(
    workspace_name: &str,
    vm_name: &str,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    {
        let mut active = tunnels.lock().await;
        if let Some(tunnel) = active.get_mut(workspace_name) {
            if tunnel_is_healthy(&tunnel.health_url, HERMES_HEALTH_PATH).await {
                return Ok(tunnel.url.clone());
            }
            let _ = tunnel.ssh_child.kill().await;
            active.remove(workspace_name);
        }
    }

    let host_port = reserve_host_port().await?;
    let tunnel_bind_host = service_tunnel_bind_host();
    let health_url = format!("http://127.0.0.1:{host_port}");
    let public_url = service_tunnel_public_url(&tunnel_bind_host, host_port)?;
    let vm = running_vm_owned(vm_name).await?;
    ensure_hermes_dashboard(&vm).await?;
    let tunnel = start_hermes_tunnel(workspace_name, &vm, &tunnel_bind_host, host_port).await?;
    wait_for_hermes_tunnel(workspace_name, tunnel, &health_url, &public_url, tunnels).await
}

async fn start_hermes_tunnel(
    workspace_name: &str,
    vm: &GuestVm,
    tunnel_bind_host: &str,
    host_port: u16,
) -> Result<ServiceTunnel> {
    let ssh_child = vm
        .forward_tcp(tunnel_bind_host, host_port, HERMES_GUEST_PORT)
        .await?;
    let _ = workspace_name;

    Ok(ServiceTunnel {
        url: service_tunnel_public_url(tunnel_bind_host, host_port)?,
        health_url: format!("http://127.0.0.1:{host_port}"),
        _vm: vm.clone(),
        ssh_child,
    })
}

async fn wait_for_hermes_tunnel(
    workspace_name: &str,
    mut tunnel: ServiceTunnel,
    health_url: &str,
    public_url: &str,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<String> {
    for _ in 0..50 {
        if tunnel_is_healthy(health_url, HERMES_HEALTH_PATH).await {
            tunnels
                .lock()
                .await
                .insert(workspace_name.to_string(), tunnel);
            return Ok(public_url.to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let _ = tunnel.ssh_child.kill().await;
    let ssh_status = tunnel
        .ssh_child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("ssh exited with {status}"))
        .unwrap_or_else(|| "ssh was still running".to_string());
    bail!("Hermes tunnel did not become reachable at {public_url}; {ssh_status}");
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

async fn running_vm_owned(name: &str) -> Result<GuestVm> {
    let handle = get_vm(name)
        .await
        .with_context(|| format!("find VM '{name}'"))?;
    match handle.status() {
        VmStatus::Running | VmStatus::Draining => handle
            .connect_with_timeout(std::time::Duration::from_secs(30))
            .await
            .with_context(|| format!("connect to running VM '{name}'")),
        VmStatus::Stopped | VmStatus::Crashed | VmStatus::Paused | VmStatus::Unknown => handle
            .start()
            .await
            .with_context(|| format!("start VM '{name}'")),
        VmStatus::Missing => bail!("VM {name} does not exist"),
    }
}

async fn ensure_hermes_dashboard(vm: &GuestVm) -> Result<()> {
    checked_shell(vm, &hermes_dashboard_script()).await
}

fn hermes_dashboard_script() -> String {
    let log_dir = HERMES_LOG_PATH
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("/tmp");

    format!(
        r#"
set -eu
if ! command -v hermes >/dev/null 2>&1; then
  echo "Hermes is not installed in this VM; recreate it with the current runtime" >&2
  exit 1
fi
mkdir -p {workdir_q} {log_dir_q}
if wget -q -O /dev/null --timeout=2 http://127.0.0.1:{port}{health_path} >/dev/null 2>&1; then
  exit 0
fi
cd {workdir_q}
if ! netstat -ltn 2>/dev/null | grep -q ':{port}[[:space:]]'; then
  setsid hermes dashboard --host 0.0.0.0 --port {port} --no-open --insecure </dev/null >{log_path_q} 2>&1 &
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
        workdir_q = shell_quote(HERMES_WORKDIR),
        log_dir_q = shell_quote(log_dir),
        port = HERMES_GUEST_PORT,
        health_path = HERMES_HEALTH_PATH,
        readiness_attempts = HERMES_READINESS_ATTEMPTS,
        log_path_q = shell_quote(HERMES_LOG_PATH),
    )
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

fn service_tunnel_public_url(bind_host: &str, port: u16) -> Result<String> {
    if let Ok(base) = env::var("MOM_SERVICE_TUNNEL_BASE_URL") {
        return service_tunnel_public_url_from_base(bind_host, port, Some(&base));
    }
    service_tunnel_public_url_from_base(bind_host, port, None)
}

fn service_tunnel_public_url_from_base(
    bind_host: &str,
    port: u16,
    base: Option<&str>,
) -> Result<String> {
    if let Some(base) = base {
        if base.contains("{port}") {
            return Ok(base.replace("{port}", &port.to_string()));
        }
        bail!("MOM_SERVICE_TUNNEL_BASE_URL must include {{port}}");
    }
    let host = if bind_host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind_host
    };
    Ok(format!("http://{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::service_tunnel_public_url_from_base;

    #[test]
    fn service_tunnel_url_supports_port_template() {
        let url = service_tunnel_public_url_from_base(
            "0.0.0.0",
            45887,
            Some("https://example.test/tunnels/{port}/"),
        )
        .unwrap();
        assert_eq!(url, "https://example.test/tunnels/45887/");
    }

    #[test]
    fn service_tunnel_url_supports_port_in_hostname() {
        let url = service_tunnel_public_url_from_base(
            "0.0.0.0",
            45887,
            Some("https://mom-1-{port}.agentmom.xyz/"),
        )
        .unwrap();
        assert_eq!(url, "https://mom-1-45887.agentmom.xyz/");
    }

    #[test]
    fn service_tunnel_url_requires_port_template() {
        let url =
            service_tunnel_public_url_from_base("0.0.0.0", 45887, Some("http://100.81.250.67"));
        assert!(url.is_err());
    }
}
