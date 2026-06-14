use std::{
    collections::{HashMap, HashSet},
    env,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, sync::Mutex};

use crate::{GuestVm, HERMES_GUEST_PORT, VmStatus, checked_shell, get_vm, normalize_preview_path};

struct ServiceTunnel {
    url: String,
    health_url: String,
    _vm: GuestVm,
    ssh_child: tokio::process::Child,
}

const HERMES_HEALTH_PATH: &str = "/api/status";

pub(crate) struct HermesDashboardUrls {
    pub(crate) public_url: String,
    pub(crate) local_url: String,
}

struct PreviewTunnelTarget<'a> {
    service_name: &'a str,
    host: &'a str,
    port: u16,
    path: &'a str,
}

#[derive(Clone, Default)]
pub(crate) struct ServiceState {
    hermes_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
    port_reservations: Arc<StdMutex<HashSet<u16>>>,
    preview_tunnels: Arc<Mutex<HashMap<String, ServiceTunnel>>>,
}

pub(crate) async fn open_hermes_dashboard(
    state: &ServiceState,
    workspace_name: &str,
    vm_name: &str,
) -> Result<String> {
    Ok(open_hermes_dashboard_urls(state, workspace_name, vm_name)
        .await?
        .public_url)
}

pub(crate) async fn open_hermes_dashboard_urls(
    state: &ServiceState,
    workspace_name: &str,
    vm_name: &str,
) -> Result<HermesDashboardUrls> {
    ensure_hermes_tunnel(
        workspace_name,
        vm_name,
        &state.hermes_tunnels,
        &state.port_reservations,
    )
    .await
}

pub(crate) async fn open_preview_app(
    state: &ServiceState,
    workspace_name: &str,
    vm_name: &str,
    service_name: &str,
    target_host: &str,
    target_port: u16,
    path: &str,
) -> Result<String> {
    ensure_preview_tunnel(
        workspace_name,
        vm_name,
        PreviewTunnelTarget {
            service_name,
            host: target_host,
            port: target_port,
            path,
        },
        &state.preview_tunnels,
        &state.port_reservations,
    )
    .await
}

pub(crate) async fn close_preview_app(
    state: &ServiceState,
    workspace_name: &str,
    service_name: &str,
) -> Result<bool> {
    let key = preview_key(workspace_name, service_name);
    let tunnel = state.preview_tunnels.lock().await.remove(&key);
    if let Some(tunnel) = tunnel {
        stop_tunnel(tunnel).await;
        return Ok(true);
    }
    Ok(false)
}

async fn ensure_hermes_tunnel(
    workspace_name: &str,
    vm_name: &str,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
    port_reservations: &Arc<StdMutex<HashSet<u16>>>,
) -> Result<HermesDashboardUrls> {
    {
        let mut active = tunnels.lock().await;
        if let Some(tunnel) = active.get_mut(workspace_name) {
            if tunnel_is_healthy(&tunnel.health_url, HERMES_HEALTH_PATH).await {
                return Ok(HermesDashboardUrls {
                    public_url: tunnel.url.clone(),
                    local_url: tunnel.health_url.clone(),
                });
            }
            let _ = tunnel.ssh_child.kill().await;
            active.remove(workspace_name);
        }
    }

    let tunnel_bind_host = service_tunnel_bind_host();
    let reservation = reserve_host_port(&tunnel_bind_host, port_reservations).await?;
    let host_port = reservation.port();
    let health_url = service_tunnel_health_url(&tunnel_bind_host, host_port);
    let public_url = service_tunnel_public_url(&tunnel_bind_host, host_port)?;
    let vm = running_vm_owned(vm_name).await?;
    ensure_hermes_dashboard(&vm).await?;
    let tunnel = start_hermes_tunnel(workspace_name, &vm, &tunnel_bind_host, host_port).await?;
    wait_for_hermes_tunnel(
        workspace_name,
        tunnel,
        reservation,
        &health_url,
        &public_url,
        tunnels,
    )
    .await
}

async fn ensure_preview_tunnel(
    workspace_name: &str,
    vm_name: &str,
    target: PreviewTunnelTarget<'_>,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
    port_reservations: &Arc<StdMutex<HashSet<u16>>>,
) -> Result<String> {
    let key = preview_key(workspace_name, target.service_name);
    let normalized_path = normalize_preview_path(target.path);
    let stale = {
        let mut active = tunnels.lock().await;
        if let Some(tunnel) = active.get_mut(&key)
            && tunnel_is_running(tunnel)
        {
            return Ok(service_url_with_path(&tunnel.url, &normalized_path));
        }
        active.remove(&key)
    };
    if let Some(tunnel) = stale {
        stop_tunnel(tunnel).await;
    }

    let tunnel_bind_host = service_tunnel_bind_host();
    let reservation = reserve_host_port(&tunnel_bind_host, port_reservations).await?;
    let host_port = reservation.port();
    let public_url = service_tunnel_public_url(&tunnel_bind_host, host_port)?;
    let vm = running_vm_owned(vm_name).await?;
    let tunnel =
        start_tcp_tunnel(&vm, &tunnel_bind_host, host_port, target.host, target.port).await?;
    reservation.release();
    let url = service_url_with_path(&public_url, &normalized_path);
    tunnels.lock().await.insert(key, tunnel);
    Ok(url)
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
        health_url: service_tunnel_health_url(tunnel_bind_host, host_port),
        _vm: vm.clone(),
        ssh_child,
    })
}

async fn start_tcp_tunnel(
    vm: &GuestVm,
    tunnel_bind_host: &str,
    host_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<ServiceTunnel> {
    let ssh_child = vm
        .forward_tcp_to(tunnel_bind_host, host_port, target_host, target_port)
        .await?;
    Ok(ServiceTunnel {
        url: service_tunnel_public_url(tunnel_bind_host, host_port)?,
        health_url: service_tunnel_health_url(tunnel_bind_host, host_port),
        _vm: vm.clone(),
        ssh_child,
    })
}

async fn wait_for_hermes_tunnel(
    workspace_name: &str,
    mut tunnel: ServiceTunnel,
    reservation: PortReservation,
    health_url: &str,
    public_url: &str,
    tunnels: &Arc<Mutex<HashMap<String, ServiceTunnel>>>,
) -> Result<HermesDashboardUrls> {
    for _ in 0..50 {
        if tunnel_is_healthy(health_url, HERMES_HEALTH_PATH).await {
            reservation.release();
            tunnels
                .lock()
                .await
                .insert(workspace_name.to_string(), tunnel);
            return Ok(HermesDashboardUrls {
                public_url: public_url.to_string(),
                local_url: health_url.to_string(),
            });
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

fn tunnel_is_running(tunnel: &mut ServiceTunnel) -> bool {
    matches!(tunnel.ssh_child.try_wait(), Ok(None))
}

async fn stop_tunnel(mut tunnel: ServiceTunnel) {
    let _ = tunnel.ssh_child.kill().await;
}

fn preview_key(workspace_name: &str, service_name: &str) -> String {
    format!("{workspace_name}:{service_name}")
}

fn service_url_with_path(base: &str, path: &str) -> String {
    let path = normalize_preview_path(path);
    if path == "/" {
        return base.to_string();
    }
    format!("{}{}", base.trim_end_matches('/'), path)
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
    r#"
set -e
if command -v agentmom-hermes-dashboard-start >/dev/null 2>&1; then
  exec agentmom-hermes-dashboard-start
fi
echo "agentmom-hermes-dashboard-start is missing; recreate this VM with the current Nix guest image" >&2
exit 1
"#
    .to_string()
}

struct PortReservation {
    port: u16,
    reservations: Arc<StdMutex<HashSet<u16>>>,
}

impl PortReservation {
    fn port(&self) -> u16 {
        self.port
    }

    fn release(self) {}
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        if let Ok(mut reservations) = self.reservations.lock() {
            reservations.remove(&self.port);
        }
    }
}

async fn reserve_host_port(
    bind_host: &str,
    reservations: &Arc<StdMutex<HashSet<u16>>>,
) -> Result<PortReservation> {
    if let Some((from, to)) = service_tunnel_port_range()? {
        for port in from..=to {
            if let Some(reservation) = try_reserve_port(bind_host, port, reservations).await? {
                return Ok(reservation);
            }
        }
        bail!("no free service tunnel ports in configured range {from}-{to}");
    }
    for _ in 0..32 {
        let listener = bind_service_tunnel_listener(bind_host, 0)
            .await
            .context("reserve local service tunnel port")?;
        let port = listener
            .local_addr()
            .context("read reserved service tunnel port")?
            .port();
        if let Some(reservation) = try_mark_port_reserved(port, reservations)? {
            drop(listener);
            return Ok(reservation);
        }
    }
    bail!("failed to reserve an unclaimed ephemeral service tunnel port")
}

async fn try_reserve_port(
    bind_host: &str,
    port: u16,
    reservations: &Arc<StdMutex<HashSet<u16>>>,
) -> Result<Option<PortReservation>> {
    let Some(reservation) = try_mark_port_reserved(port, reservations)? else {
        return Ok(None);
    };
    if bind_service_tunnel_listener(bind_host, port).await.is_ok() {
        Ok(Some(reservation))
    } else {
        drop(reservation);
        Ok(None)
    }
}

fn try_mark_port_reserved(
    port: u16,
    reservations: &Arc<StdMutex<HashSet<u16>>>,
) -> Result<Option<PortReservation>> {
    let mut reservations_guard = reservations
        .lock()
        .map_err(|_| anyhow::anyhow!("service tunnel port reservation lock poisoned"))?;
    if !reservations_guard.insert(port) {
        return Ok(None);
    }
    Ok(Some(PortReservation {
        port,
        reservations: Arc::clone(reservations),
    }))
}

async fn bind_service_tunnel_listener(
    bind_host: &str,
    port: u16,
) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind((bind_host, port)).await
}

fn service_tunnel_port_range() -> Result<Option<(u16, u16)>> {
    let Ok(raw) = env::var("MOM_SERVICE_TUNNEL_PORT_RANGE") else {
        return Ok(None);
    };
    parse_service_tunnel_port_range(&raw).map(Some)
}

fn parse_service_tunnel_port_range(raw: &str) -> Result<(u16, u16)> {
    let (from, to) = raw
        .split_once('-')
        .or_else(|| raw.split_once(':'))
        .ok_or_else(|| anyhow::anyhow!("MOM_SERVICE_TUNNEL_PORT_RANGE must look like from-to"))?;
    let from = from
        .parse::<u16>()
        .with_context(|| format!("parse service tunnel range start {from:?}"))?;
    let to = to
        .parse::<u16>()
        .with_context(|| format!("parse service tunnel range end {to:?}"))?;
    if from > to {
        bail!("MOM_SERVICE_TUNNEL_PORT_RANGE start must be <= end: {raw}");
    }
    Ok((from, to))
}

fn service_tunnel_bind_host() -> String {
    env::var("MOM_SERVICE_TUNNEL_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn service_tunnel_health_url(bind_host: &str, port: u16) -> String {
    format!(
        "http://{}:{port}",
        http_host_for_url(service_tunnel_probe_host(bind_host))
    )
}

fn service_tunnel_probe_host(bind_host: &str) -> &str {
    match bind_host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        _ => bind_host,
    }
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
    Ok(format!(
        "http://{}:{port}",
        http_host_for_url(service_tunnel_probe_host(bind_host))
    ))
}

fn http_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        hermes_dashboard_script, parse_service_tunnel_port_range, service_tunnel_health_url,
        service_tunnel_public_url_from_base, service_url_with_path,
    };

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

    #[test]
    fn service_tunnel_port_range_parses_bounded_range() {
        assert_eq!(
            parse_service_tunnel_port_range("40000-40010").unwrap(),
            (40000, 40010)
        );
        assert!(parse_service_tunnel_port_range("40010-40000").is_err());
    }

    #[test]
    fn service_tunnel_health_url_uses_bind_host_except_for_wildcards() {
        assert_eq!(
            service_tunnel_health_url("100.81.250.67", 45887),
            "http://100.81.250.67:45887"
        );
        assert_eq!(
            service_tunnel_health_url("0.0.0.0", 45887),
            "http://127.0.0.1:45887"
        );
        assert_eq!(service_tunnel_health_url("::", 45887), "http://[::1]:45887");
    }

    #[test]
    fn hermes_dashboard_script_calls_guest_launcher() {
        let script = hermes_dashboard_script();
        assert!(script.contains("set -e\n"));
        assert!(!script.contains("set -eu"));
        assert!(script.contains("exec agentmom-hermes-dashboard-start"));
        assert!(script.contains("recreate this VM with the current Nix guest image"));
    }

    #[test]
    fn hermes_dashboard_script_does_not_start_legacy_background_processes() {
        let script = hermes_dashboard_script();
        assert!(!script.contains("setsid"));
        assert!(!script.contains("hermes dashboard --host"));
        assert!(!script.contains("HERMES_WEB_DIST"));
    }

    #[test]
    fn hermes_dashboard_script_does_not_source_legacy_profiles() {
        let script = hermes_dashboard_script();

        assert!(script.contains("agentmom-hermes-dashboard-start"));
        assert!(!script.contains(". /etc/profile.d/mom.sh"));
        assert!(!script.contains(". /etc/profile.d/agentmom-proxy.sh"));
    }

    #[test]
    fn preview_url_appends_path_to_tunnel_base() {
        assert_eq!(
            service_url_with_path("https://mom-1-45887.agentmom.xyz", "dashboard"),
            "https://mom-1-45887.agentmom.xyz/dashboard"
        );
        assert_eq!(
            service_url_with_path("https://example.test/tunnels/45887/", "/"),
            "https://example.test/tunnels/45887/"
        );
        assert_eq!(
            service_url_with_path("https://example.test/tunnels/45887/", "/app"),
            "https://example.test/tunnels/45887/app"
        );
    }
}
