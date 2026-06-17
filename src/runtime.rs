use std::{collections::HashMap, io::Write, time::Instant};

use fs2::FileExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

const DEFAULT_MICROVM_CIDR_PREFIX: &str = "192.168.83";
const MIN_MACHINE_INDEX: u16 = 10;
const MAX_MACHINE_INDEX: u16 = 229;
const FRESH_VM_SYSTEMD_ACTIVE_TIMEOUT: Duration = Duration::from_secs(30);
const FRESH_VM_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VmStatus {
    Running,
    Draining,
    Stopped,
    Paused,
    Suspended,
    Crashed,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VmStartTiming {
    pub(crate) connect_ssh_ms: Option<u128>,
    pub(crate) refresh_definition_ms: Option<u128>,
    pub(crate) systemctl_start_ms: Option<u128>,
    pub(crate) systemd_active_ms: Option<u128>,
    pub(crate) ch_api_ms: Option<u128>,
    pub(crate) net_prime_ms: Option<u128>,
    pub(crate) ch_resume_ms: Option<u128>,
    pub(crate) tcp_22_ready_ms: Option<u128>,
    pub(crate) ssh_ready_ms: Option<u128>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmReadiness {
    Tcp22,
    Ssh,
}

impl VmStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Paused => "paused",
            Self::Suspended => "suspended",
            Self::Crashed => "crashed",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Draining)
    }

    pub(crate) fn is_started(self) -> bool {
        matches!(self, Self::Running | Self::Draining | Self::Paused)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VmHandle {
    name: String,
    status: VmStatus,
    labels: HashMap<String, String>,
}

impl VmHandle {
    pub(crate) fn status(&self) -> VmStatus {
        self.status
    }

    pub(crate) fn labels(&self) -> &HashMap<String, String> {
        &self.labels
    }

    pub(crate) async fn start(&self) -> Result<GuestVm> {
        start_vm(&self.name).await
    }

    pub(crate) async fn start_acp_timed(&self) -> Result<(GuestVm, VmStartTiming)> {
        start_vm_timed_for_acp(&self.name).await
    }

    pub(crate) async fn pause(&self) -> Result<()> {
        pause_vm(&self.name).await
    }

    pub(crate) async fn suspend(&self) -> Result<()> {
        suspend_vm(&self.name).await
    }

    pub(crate) async fn resume(&self) -> Result<GuestVm> {
        resume_vm(&self.name).await
    }

    pub(crate) async fn connect_with_timeout(&self, timeout: Duration) -> Result<GuestVm> {
        let vm = GuestVm::new(self.name.clone());
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(&vm, &spec, timeout).await?;
        Ok(vm)
    }

    pub(crate) async fn stop_with_timeout(&self, timeout: Duration) -> Result<()> {
        stop_vm_with_timeout(&self.name, timeout).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GuestVm {
    name: String,
}

impl GuestVm {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) async fn shell(&self, script: &str) -> Result<GuestOutput> {
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(self, &spec, Duration::from_secs(90)).await?;
        run_ssh_shell(&self.name, &spec, script, None).await
    }

    pub(crate) async fn spawn_shell_ready(&self, script: &str) -> Result<tokio::process::Child> {
        let spec = load_microvm_spec(&self.name)?;
        let mut command = TokioCommand::new("ssh");
        command
            .args(ssh_common_args(&self.name, &spec)?)
            .arg(ssh_destination(&spec))
            .arg(format!("/bin/sh -lc {}", shell_quote(script)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
            .spawn()
            .with_context(|| format!("start SSH command in VM {}", self.name))
    }

    pub(crate) async fn resume_if_state_file_paused_fast(&self) -> Result<bool> {
        let state = fs::read_to_string(machine_dir(&self.name)?.join("state")).unwrap_or_default();
        if state.trim() != "paused" {
            return Ok(false);
        }
        ensure_machine_exists(&self.name)?;
        cloud_hypervisor_control(&self.name, "resume").await?;
        fs::write(machine_dir(&self.name)?.join("state"), b"running\n")?;
        Ok(true)
    }

    pub(crate) async fn guest_ping(&self) -> Result<(String, u128)> {
        let spec = load_microvm_spec(&self.name)?;
        let started = Instant::now();
        let mut stream = tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect((spec.guest_ip.as_str(), 9199)),
        )
        .await
        .with_context(|| format!("guest ping connect timed out for VM {}", self.name))?
        .with_context(|| format!("connect guest ping service for VM {}", self.name))?;
        stream
            .write_all(b"ping\n")
            .await
            .with_context(|| format!("write guest ping for VM {}", self.name))?;
        let mut response = vec![0; 128];
        let read = tokio::time::timeout(Duration::from_millis(250), stream.read(&mut response))
            .await
            .with_context(|| format!("guest ping read timed out for VM {}", self.name))?
            .with_context(|| format!("read guest ping response for VM {}", self.name))?;
        Ok((
            String::from_utf8_lossy(&response[..read])
                .trim()
                .to_string(),
            started.elapsed().as_millis(),
        ))
    }

    pub(crate) async fn forward_tcp(
        &self,
        bind_host: &str,
        host_port: u16,
        guest_port: u16,
    ) -> Result<tokio::process::Child> {
        self.forward_tcp_to(bind_host, host_port, "127.0.0.1", guest_port)
            .await
    }

    pub(crate) async fn forward_tcp_to(
        &self,
        bind_host: &str,
        host_port: u16,
        target_host: &str,
        target_port: u16,
    ) -> Result<tokio::process::Child> {
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(self, &spec, Duration::from_secs(90)).await?;
        let mut command = TokioCommand::new("ssh");
        command
            .args(ssh_common_args(&self.name, &spec)?)
            .args([
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-L",
                &format!("{bind_host}:{host_port}:{target_host}:{target_port}"),
            ])
            .arg(ssh_destination(&spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .with_context(|| format!("start TCP forward for VM {}", self.name))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GuestOutput {
    pub(crate) ok: bool,
    pub(crate) code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MicrovmSpec {
    name: String,
    workspace_name: String,
    workspace_dir_name: String,
    cpus: u8,
    memory_mib: u64,
    workspace_quota_mib: u32,
    machine_index: u16,
    guest_ip: String,
    host_ip: String,
    host_bridge: String,
    tap: String,
    mac: String,
    workspace_dir: String,
    hermes_profile: String,
    hermes_model: String,
    credential_proxy_url: Option<String>,
    credential_proxy_ca_file: Option<String>,
    nixpkgs_url: String,
    microvm_input_url: String,
    hermes_agent_input_url: String,
    ssh_public_key: String,
    #[serde(default)]
    ssh_host_public_key: String,
    #[serde(default)]
    ssh_host_key_dir: String,
    labels: HashMap<String, String>,
}

pub(crate) async fn create_vm(request: WorkspaceVmRequest) -> Result<()> {
    let _lock = acquire_machine_state_lock()?;
    let config = load_mom_config()?;
    config.validate_for_node()?;
    let name = request.name.clone();
    if request.replace {
        if machine_dir(&name)?.exists() {
            remove_vm(&name).await?;
        }
    } else if machine_dir(&name)?.exists() {
        bail!("VM {name} already exists; pass --replace to recreate it");
    }

    let dir = machine_dir(&name)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let ssh_public_key = generate_ssh_keypair(&dir, &name).await?;
    let (ssh_host_public_key, ssh_host_key_dir) = ensure_ssh_host_keypair(&dir, &name).await?;
    let spec = microvm_spec(
        &request,
        &config,
        ssh_public_key,
        ssh_host_public_key,
        ssh_host_key_dir,
    )?;
    fs::create_dir_all(&spec.workspace_dir)
        .with_context(|| format!("create workspace dir {}", spec.workspace_dir))?;

    write_vm_definition(&dir, &spec, &config)?;
    fs::write(dir.join("state"), b"stopped\n")?;

    println!(
        "created declarative microvm.nix VM {name} in {}",
        dir.display()
    );
    Ok(())
}

pub(crate) async fn get_vm(name: &str) -> Result<VmHandle> {
    let dir = machine_dir(name)?;
    if !dir.exists() {
        bail!("VM {name} does not exist");
    }
    let spec = load_microvm_spec(name)?;
    Ok(VmHandle {
        name: name.to_string(),
        status: vm_status(name).await?,
        labels: spec.labels,
    })
}

pub(crate) async fn list_vms() -> Result<Vec<VmHandle>> {
    let root = machines_dir()?;
    let mut vms = Vec::new();
    if !root.exists() {
        return Ok(vms);
    }
    for entry in fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.path().join("spec.json").exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(handle) = get_vm(&name).await {
            vms.push(handle);
        }
    }
    Ok(vms)
}

pub(crate) async fn start_vm(name: &str) -> Result<GuestVm> {
    start_vm_timed(name).await.map(|(vm, _)| vm)
}

pub(crate) async fn start_vm_timed(name: &str) -> Result<(GuestVm, VmStartTiming)> {
    start_vm_timed_with_readiness(name, VmReadiness::Ssh).await
}

pub(crate) async fn start_vm_timed_for_acp(name: &str) -> Result<(GuestVm, VmStartTiming)> {
    start_vm_timed_with_readiness(name, VmReadiness::Tcp22).await
}

async fn start_vm_timed_with_readiness(
    name: &str,
    readiness: VmReadiness,
) -> Result<(GuestVm, VmStartTiming)> {
    ensure_machine_exists(name)?;
    let mut timing = VmStartTiming::default();
    match vm_status(name).await? {
        VmStatus::Running | VmStatus::Draining => {
            let vm = GuestVm::new(name);
            let spec = load_microvm_spec(name)?;
            let started = Instant::now();
            wait_for_vm_readiness(&vm, &spec, Duration::from_secs(90), readiness, &mut timing)
                .await?;
            if readiness == VmReadiness::Ssh {
                timing.connect_ssh_ms = Some(started.elapsed().as_millis());
            }
            return Ok((vm, timing));
        }
        VmStatus::Paused => {
            let started = Instant::now();
            cloud_hypervisor_control(name, "resume").await?;
            timing.ch_resume_ms = Some(started.elapsed().as_millis());
            fs::write(machine_dir(name)?.join("state"), b"running\n")?;
            let vm = GuestVm::new(name);
            let spec = load_microvm_spec(name)?;
            wait_for_vm_readiness(&vm, &spec, Duration::from_secs(30), readiness, &mut timing)
                .await?;
            return Ok((vm, timing));
        }
        VmStatus::Suspended => {
            return restore_suspended_vm_timed_with_readiness(name, readiness).await;
        }
        VmStatus::Stopped | VmStatus::Crashed | VmStatus::Missing | VmStatus::Unknown => {}
    }
    let started = Instant::now();
    refresh_vm_definition(name)?;
    timing.refresh_definition_ms = Some(started.elapsed().as_millis());
    let started = Instant::now();
    systemctl(&["start", &microvm_systemd_unit(name)]).await?;
    timing.systemctl_start_ms = Some(started.elapsed().as_millis());
    let vm = GuestVm::new(name);
    let spec = load_microvm_spec(name)?;
    let started = Instant::now();
    if let Err(error) = wait_for_systemd_active(name, FRESH_VM_SYSTEMD_ACTIVE_TIMEOUT).await {
        let diagnostics = microvm_unit_diagnostics(name).await;
        return Err(error.context(format!(
            "microVM unit diagnostics for {name}:\n{diagnostics}"
        )));
    }
    timing.systemd_active_ms = Some(started.elapsed().as_millis());
    if let Err(error) = wait_for_vm_readiness(
        &vm,
        &spec,
        FRESH_VM_SSH_READY_TIMEOUT,
        readiness,
        &mut timing,
    )
    .await
    {
        let diagnostics = microvm_unit_diagnostics(name).await;
        return Err(error.context(format!(
            "microVM unit diagnostics for {name}:\n{diagnostics}"
        )));
    }
    fs::write(machine_dir(name)?.join("state"), b"running\n")?;
    Ok((vm, timing))
}

pub(crate) async fn pause_vm(name: &str) -> Result<()> {
    ensure_machine_exists(name)?;
    match vm_status(name).await? {
        VmStatus::Paused => return Ok(()),
        VmStatus::Running | VmStatus::Draining => {}
        status => bail!("VM {name} is {}, not running", status.as_str()),
    }
    cloud_hypervisor_control(name, "pause").await?;
    fs::write(machine_dir(name)?.join("state"), b"paused\n")?;
    Ok(())
}

pub(crate) async fn suspend_vm(name: &str) -> Result<()> {
    ensure_machine_exists(name)?;
    let status = vm_status(name).await?;
    match status {
        VmStatus::Suspended => return Ok(()),
        VmStatus::Paused => {}
        VmStatus::Running | VmStatus::Draining => {
            cloud_hypervisor_control(name, "pause").await?;
        }
        status => bail!("VM {name} is {}, not running or paused", status.as_str()),
    }

    let dir = machine_dir(name)?;
    let snapshot_dir = dir.join("snapshot");
    if snapshot_dir.exists() {
        remove_runtime_dir_all(&snapshot_dir).await?;
    }
    fs::create_dir_all(&snapshot_dir)
        .with_context(|| format!("create snapshot dir {}", snapshot_dir.display()))?;
    let snapshot_url = format!(
        "file://{}",
        snapshot_dir
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 snapshot path {}", snapshot_dir.display()))?
    );
    cloud_hypervisor_control_args(name, &["snapshot", &snapshot_url]).await?;
    fs::write(dir.join("state"), b"suspended\n")?;
    stop_vm_process(name, Duration::from_secs(20)).await?;
    Ok(())
}

pub(crate) async fn restore_suspended_vm(name: &str) -> Result<GuestVm> {
    restore_suspended_vm_timed(name).await.map(|(vm, _)| vm)
}

pub(crate) async fn restore_suspended_vm_timed(name: &str) -> Result<(GuestVm, VmStartTiming)> {
    restore_suspended_vm_timed_with_readiness(name, VmReadiness::Ssh).await
}

async fn restore_suspended_vm_timed_with_readiness(
    name: &str,
    readiness: VmReadiness,
) -> Result<(GuestVm, VmStartTiming)> {
    ensure_machine_exists(name)?;
    let mut timing = VmStartTiming::default();
    match vm_status(name).await? {
        VmStatus::Running | VmStatus::Draining => {
            let vm = GuestVm::new(name);
            let spec = load_microvm_spec(name)?;
            let started = Instant::now();
            wait_for_vm_readiness(&vm, &spec, Duration::from_secs(90), readiness, &mut timing)
                .await?;
            if readiness == VmReadiness::Ssh {
                timing.connect_ssh_ms = Some(started.elapsed().as_millis());
            }
            return Ok((vm, timing));
        }
        VmStatus::Paused => {
            let started = Instant::now();
            cloud_hypervisor_control(name, "resume").await?;
            timing.ch_resume_ms = Some(started.elapsed().as_millis());
            fs::write(machine_dir(name)?.join("state"), b"running\n")?;
            let vm = GuestVm::new(name);
            let spec = load_microvm_spec(name)?;
            wait_for_vm_readiness(&vm, &spec, Duration::from_secs(30), readiness, &mut timing)
                .await?;
            return Ok((vm, timing));
        }
        VmStatus::Suspended => {}
        status @ (VmStatus::Stopped | VmStatus::Crashed | VmStatus::Unknown) => {
            bail!("VM {name} is {}, not suspended", status.as_str())
        }
        VmStatus::Missing => bail!("VM {name} does not exist"),
    }
    let snapshot_dir = machine_dir(name)?.join("snapshot");
    if !snapshot_dir.exists() {
        bail!(
            "VM {name} is suspended but snapshot dir is missing: {}",
            snapshot_dir.display()
        );
    }
    let started = Instant::now();
    refresh_vm_definition(name)?;
    timing.refresh_definition_ms = Some(started.elapsed().as_millis());
    let started = Instant::now();
    systemctl(&["start", &microvm_systemd_unit(name)]).await?;
    timing.systemctl_start_ms = Some(started.elapsed().as_millis());
    let started = Instant::now();
    if let Err(error) = wait_for_systemd_active(name, FRESH_VM_SYSTEMD_ACTIVE_TIMEOUT).await {
        let diagnostics = microvm_unit_diagnostics(name).await;
        return Err(error.context(format!(
            "microVM unit diagnostics for {name}:\n{diagnostics}"
        )));
    }
    timing.systemd_active_ms = Some(started.elapsed().as_millis());
    let started = Instant::now();
    wait_for_cloud_hypervisor_api(name, Duration::from_secs(30)).await?;
    timing.ch_api_ms = Some(started.elapsed().as_millis());
    let vm = GuestVm::new(name);
    let spec = load_microvm_spec(name)?;
    let started = Instant::now();
    prime_guest_network(&spec).await;
    timing.net_prime_ms = Some(started.elapsed().as_millis());
    wait_for_vm_readiness(&vm, &spec, Duration::from_secs(30), readiness, &mut timing).await?;
    fs::write(machine_dir(name)?.join("state"), b"running\n")?;
    Ok((vm, timing))
}

pub(crate) async fn resume_vm(name: &str) -> Result<GuestVm> {
    ensure_machine_exists(name)?;
    match vm_status(name).await? {
        VmStatus::Running | VmStatus::Draining => {
            let vm = GuestVm::new(name);
            let spec = load_microvm_spec(name)?;
            wait_for_ssh(&vm, &spec, Duration::from_secs(90)).await?;
            return Ok(vm);
        }
        VmStatus::Suspended => return restore_suspended_vm(name).await,
        VmStatus::Paused => {}
        status @ (VmStatus::Stopped | VmStatus::Crashed | VmStatus::Unknown) => {
            bail!("VM {name} is {}, not paused", status.as_str())
        }
        VmStatus::Missing => bail!("VM {name} does not exist"),
    }
    cloud_hypervisor_control(name, "resume").await?;
    fs::write(machine_dir(name)?.join("state"), b"running\n")?;
    let vm = GuestVm::new(name);
    let spec = load_microvm_spec(name)?;
    wait_for_tcp_port(&spec.guest_ip, 22, Duration::from_secs(30)).await?;
    wait_for_ssh(&vm, &spec, Duration::from_secs(30)).await?;
    Ok(vm)
}

pub(crate) async fn stop_vm(name: &str) -> Result<()> {
    stop_vm_with_timeout(name, Duration::from_secs(20)).await
}

pub(crate) async fn stop_vm_with_timeout(name: &str, timeout: Duration) -> Result<()> {
    ensure_machine_exists(name)?;
    stop_vm_process(name, timeout).await?;
    let snapshot_dir = machine_dir(name)?.join("snapshot");
    if snapshot_dir.exists() {
        remove_runtime_dir_all(&snapshot_dir).await?;
    }
    fs::write(machine_dir(name)?.join("state"), b"stopped\n")?;
    Ok(())
}

async fn stop_vm_process(name: &str, timeout: Duration) -> Result<()> {
    let unit = microvm_systemd_unit(name);
    systemctl(&["stop", "--job-mode=replace", &unit]).await?;
    wait_for_systemd_inactive(name, timeout).await?;
    Ok(())
}

pub(crate) async fn remove_vm(name: &str) -> Result<()> {
    if machine_dir(name)?.exists() {
        stop_vm(name).await?;
    }
    let dir = machine_dir(name)?;
    if dir.exists() {
        remove_runtime_dir_all(&dir).await?;
    }
    Ok(())
}

pub(crate) async fn remove_workspace_dir(workspace_dir_name: &str) -> Result<()> {
    let path = workspace_dir_path(workspace_dir_name)?;
    if path.exists() {
        remove_runtime_dir_all(&path).await?;
    }
    Ok(())
}

pub(crate) async fn vm_status(name: &str) -> Result<VmStatus> {
    if !machine_dir(name)?.exists() {
        return Ok(VmStatus::Missing);
    }
    if command_exists("systemctl").await {
        let unit = microvm_systemd_unit(name);
        let output = TokioCommand::new("systemctl")
            .args(["is-active", &unit])
            .stdin(Stdio::null())
            .output()
            .await;
        if let Ok(output) = output {
            let active = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let state = fs::read_to_string(machine_dir(name)?.join("state")).unwrap_or_default();
            return Ok(vm_status_from_systemd(&active, state.trim()));
        }
    }
    let state = fs::read_to_string(machine_dir(name)?.join("state")).unwrap_or_default();
    Ok(match state.trim() {
        "running" => VmStatus::Running,
        "stopped" => VmStatus::Stopped,
        "paused" => VmStatus::Paused,
        "suspended" => VmStatus::Suspended,
        "crashed" => VmStatus::Crashed,
        _ => VmStatus::Unknown,
    })
}

fn vm_status_from_systemd(active: &str, state: &str) -> VmStatus {
    match active {
        "active" | "activating" if state == "paused" => VmStatus::Paused,
        "active" | "activating" if state == "suspended" => VmStatus::Draining,
        "active" | "activating" if state == "running" => VmStatus::Running,
        "active" | "activating" if state == "stopped" => VmStatus::Stopped,
        "active" | "activating" => VmStatus::Unknown,
        "deactivating" => VmStatus::Draining,
        "failed" => VmStatus::Crashed,
        "inactive" if state == "suspended" => VmStatus::Suspended,
        "inactive" => VmStatus::Stopped,
        _ => VmStatus::Unknown,
    }
}

pub(crate) async fn ensure_runtime(config: &MomConfig) -> Result<()> {
    ensure_microvm_host_ready(config).await
}

pub(crate) async fn ensure_runtime_for_deploy(config: &MomConfig) -> Result<()> {
    config.validate_for_node()?;
    ensure_runtime(config).await?;
    println!("microvm.nix runtime host checks passed");
    Ok(())
}

pub(crate) async fn proxy_smoke(vm: &GuestVm) -> Result<()> {
    checked_shell(
        vm,
        r#"
set -e
test -f /etc/profile.d/agentmom-proxy.sh
. /etc/profile.d/agentmom-proxy.sh
test -n "${HTTPS_PROXY:-}"
test "${OPENROUTER_API_KEY:-}" = "agentmom-proxy"
python3 - <<'PY'
import json
import ssl
import urllib.request

request = urllib.request.Request("https://openrouter.ai/api/v1/models")
with urllib.request.urlopen(request, timeout=20, context=ssl.create_default_context()) as response:
    payload = json.load(response)
if "data" not in payload:
    raise SystemExit("OpenRouter models response did not include data")
print("proxy smoke ok")
PY
"#,
    )
    .await
}

pub(crate) async fn run_guest_command(vm: &GuestVm, command: Vec<String>) -> Result<()> {
    let output = capture_guest_command(vm, command).await?;
    print!("{}", output["stdout"].as_str().unwrap_or_default());
    eprint!("{}", output["stderr"].as_str().unwrap_or_default());
    if !output["ok"].as_bool().unwrap_or(false) {
        bail!(
            "guest command exited with {}",
            output["code"].as_i64().unwrap_or_default()
        );
    }
    Ok(())
}

pub(crate) async fn capture_guest_command(vm: &GuestVm, command: Vec<String>) -> Result<Value> {
    let script = guest_command_script(&command)?;
    let output = vm.shell(&script).await?;
    Ok(json!({
        "ok": output.ok,
        "code": output.code,
        "stdout": output.stdout,
        "stderr": output.stderr
    }))
}

fn guest_command_script(command: &[String]) -> Result<String> {
    if command.is_empty() {
        bail!("guest command cannot be empty");
    }
    let command_text = quoted_command(command);
    let fallback_command = hermes_wrapper_fallback_command(command);
    let runner = shell_quote(GUEST_AGENTMOM_RUN);
    let hermes = shell_quote(GUEST_AGENTMOM_HERMES);
    let wrapper_fallback = match fallback_command.as_deref() {
        Some(fallback) => format!(
            "\
  if [ -x {hermes} ]; then
    exec {runner} -- {command_text}
  fi
  exec {runner} -- {fallback}
",
        ),
        None => format!("  exec {runner} -- {command_text}\n"),
    };
    let fallback_exec = fallback_command.as_deref().unwrap_or(&command_text);
    Ok(format!(
        "\
set -e
if [ -x {runner} ]; then
{wrapper_fallback}fi
if [ -x {hermes} ]; then
  exec {command_text}
fi
if [ -f /etc/profile.d/mom.sh ]; then . /etc/profile.d/mom.sh; fi
if [ -f /etc/profile.d/agentmom-proxy.sh ]; then . /etc/profile.d/agentmom-proxy.sh; fi
export HOME=/root
cd /workspace
exec {fallback_exec}
"
    ))
}

fn quoted_command(command: &[String]) -> String {
    command
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hermes_wrapper_fallback_command(command: &[String]) -> Option<String> {
    if command.first().map(String::as_str) != Some(GUEST_AGENTMOM_HERMES) {
        return None;
    }
    let mut fallback = vec!["hermes".to_string()];
    fallback.extend(command.iter().skip(1).cloned());
    Some(quoted_command(&fallback))
}

pub(crate) fn workspace_dir_path(workspace_dir_name: &str) -> Result<PathBuf> {
    Ok(workspace_dir_root()?.join(workspace_dir_name))
}

pub(crate) fn runtime_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_MICROVM_STATE_DIR") {
        return absolute_path(PathBuf::from(path));
    }
    absolute_path(fleet_state_dir()?.join("microvms"))
}

pub(crate) fn machines_dir() -> Result<PathBuf> {
    Ok(runtime_home()?.join("machines"))
}

fn workspace_dir_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_MICROVM_WORKSPACE_DIR") {
        return absolute_path(PathBuf::from(path));
    }
    absolute_path(runtime_home()?.join("workspaces"))
}

fn machine_dir(name: &str) -> Result<PathBuf> {
    Ok(machines_dir()?.join(name))
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn ensure_machine_exists(name: &str) -> Result<()> {
    let dir = machine_dir(name)?;
    if !dir.exists() {
        bail!("VM {name} does not exist; expected {}", dir.display());
    }
    Ok(())
}

fn load_microvm_spec(name: &str) -> Result<MicrovmSpec> {
    let path = machine_dir(name)?.join("spec.json");
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn refresh_vm_definition(name: &str) -> Result<()> {
    let _lock = acquire_machine_state_lock()?;
    let config = load_mom_config()?;
    config.validate_for_node()?;
    let dir = machine_dir(name)?;
    let mut spec = load_microvm_spec(name)?;
    validate_workspace_source(name, &spec)?;
    ensure_spec_ssh_host_identity(&dir, name, &mut spec)?;
    apply_current_microvm_config(&mut spec, &config)?;
    write_vm_definition(&dir, &spec, &config)
}

fn write_vm_definition(dir: &Path, spec: &MicrovmSpec, config: &MomConfig) -> Result<()> {
    validate_ssh_host_identity(spec)?;
    if let Some(ca_path) = &config.credentials.proxy_ca_path {
        sync_proxy_ca_file(dir, ca_path)?;
    }
    write_file_if_changed(&dir.join("spec.json"), &serde_json::to_vec_pretty(spec)?)?;
    write_file_if_changed(
        dir.join("microvm-workspace.nix").as_path(),
        microvm_workspace_nix().as_bytes(),
    )?;
    write_file_if_changed(
        dir.join("hermes-agent-package.nix").as_path(),
        hermes_agent_package_nix().as_bytes(),
    )?;
    write_file_if_changed(&dir.join("flake.nix"), microvm_flake_nix(spec)?.as_bytes())?;
    write_file_if_changed(&dir.join("known_hosts"), known_hosts_entry(spec).as_bytes())?;
    if config.credentials.proxy_ca_path.is_none() {
        remove_file_if_exists(&dir.join("agentmom-proxy.crt"))?;
    }
    Ok(())
}

fn validate_workspace_source(name: &str, spec: &MicrovmSpec) -> Result<()> {
    let stored = PathBuf::from(&spec.workspace_dir);
    let current = workspace_dir_path(&spec.workspace_dir_name)?;
    if stored != current {
        bail!(
            "refusing to rewrite workspace source for VM {name}: stored path {} resolves to {} under current config",
            stored.display(),
            current.display()
        );
    }
    if !stored.exists() {
        bail!(
            "workspace source for VM {name} is missing: {}",
            stored.display()
        );
    }
    if !stored.is_dir() {
        bail!(
            "workspace source for VM {name} is not a directory: {}",
            stored.display()
        );
    }
    Ok(())
}

fn sync_proxy_ca_file(dir: &Path, ca_path: &Path) -> Result<()> {
    let ca_dest = dir.join("agentmom-proxy.crt");
    let ca_path = resolve_required_file(ca_path, "credentials.proxy_ca_path")?;
    let bytes =
        fs::read(&ca_path).with_context(|| format!("read proxy CA {}", ca_path.display()))?;
    write_file_if_changed(&ca_dest, &bytes).with_context(|| {
        format!(
            "copy proxy CA {} to {}",
            ca_path.display(),
            ca_dest.display()
        )
    })
}

fn write_file_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Ok(existing) = fs::read(path)
        && existing == bytes
    {
        return Ok(());
    }
    atomic_write_file(path, bytes)
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?
        .to_string_lossy();
    let temp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        unique_suffix()?
    ));

    let write_result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("create temp file {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write temp file {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp file {}", temp.display()))?;
        drop(file);
        fs::rename(&temp, path)
            .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
        sync_parent_dir(path)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result.with_context(|| format!("write {}", path.display()))
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path).with_context(|| format!("remove {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let dir = fs::File::open(parent).with_context(|| format!("open {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("sync {}", parent.display()))
}

fn unique_suffix() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_nanos())
}

fn microvm_spec(
    request: &WorkspaceVmRequest,
    config: &MomConfig,
    ssh_public_key: String,
    ssh_host_public_key: String,
    ssh_host_key_dir: String,
) -> Result<MicrovmSpec> {
    let workspace_dir = workspace_dir_path(&request.workspace_dir_name)?;
    let index = allocate_machine_index(&request.name)?;
    let prefix = microvm_cidr_prefix();
    let host_ip = env::var("MOM_MICROVM_HOST_IP").unwrap_or_else(|_| format!("{prefix}.1"));
    let host_bridge = env::var("MOM_MICROVM_BRIDGE").unwrap_or_else(|_| "agentmom0".to_string());
    let tap_prefix = env::var("MOM_MICROVM_TAP_PREFIX").unwrap_or_else(|_| "amvm".to_string());
    let mut labels = HashMap::new();
    labels.insert("mom.workspace".to_string(), request.workspace_name.clone());

    let mut spec = MicrovmSpec {
        name: request.name.clone(),
        workspace_name: request.workspace_name.clone(),
        workspace_dir_name: request.workspace_dir_name.clone(),
        cpus: request.cpus,
        memory_mib: request.memory_mib,
        workspace_quota_mib: request.workspace_quota_mib,
        machine_index: index,
        guest_ip: format!("{prefix}.{index}"),
        host_ip,
        host_bridge,
        tap: format!("{tap_prefix}{index}"),
        mac: format!("02:00:00:83:{:02x}:{:02x}", index / 256, index % 256),
        workspace_dir: workspace_dir.display().to_string(),
        hermes_profile: String::new(),
        hermes_model: String::new(),
        credential_proxy_url: None,
        credential_proxy_ca_file: None,
        nixpkgs_url: String::new(),
        microvm_input_url: String::new(),
        hermes_agent_input_url: String::new(),
        ssh_public_key,
        ssh_host_public_key,
        ssh_host_key_dir,
        labels,
    };
    apply_current_microvm_config(&mut spec, config)?;
    Ok(spec)
}

fn apply_current_microvm_config(spec: &mut MicrovmSpec, config: &MomConfig) -> Result<()> {
    let prefix = microvm_cidr_prefix();
    let host_ip = env::var("MOM_MICROVM_HOST_IP").unwrap_or_else(|_| format!("{prefix}.1"));
    let host_bridge = env::var("MOM_MICROVM_BRIDGE").unwrap_or_else(|_| "agentmom0".to_string());
    let tap_prefix = env::var("MOM_MICROVM_TAP_PREFIX").unwrap_or_else(|_| "amvm".to_string());
    spec.guest_ip = format!("{prefix}.{}", spec.machine_index);
    spec.host_ip = host_ip;
    spec.host_bridge = host_bridge;
    spec.tap = format!("{tap_prefix}{}", spec.machine_index);
    spec.mac = format!(
        "02:00:00:83:{:02x}:{:02x}",
        spec.machine_index / 256,
        spec.machine_index % 256
    );
    spec.workspace_dir = workspace_dir_path(&spec.workspace_dir_name)?
        .display()
        .to_string();
    spec.hermes_profile = config.hermes_profile().to_string();
    spec.hermes_model = config.model().to_string();
    spec.credential_proxy_url = config.credential_proxy_url().map(ToString::to_string);
    spec.credential_proxy_ca_file = config
        .credentials
        .proxy_ca_path
        .as_ref()
        .map(|_| "agentmom-proxy.crt".to_string());
    spec.nixpkgs_url = env::var("MOM_MICROVM_NIXPKGS_URL")
        .unwrap_or_else(|_| "github:NixOS/nixpkgs/nixpkgs-unstable".to_string());
    spec.microvm_input_url = env::var("MOM_MICROVM_NIX_URL")
        .unwrap_or_else(|_| "github:microvm-nix/microvm.nix".to_string());
    spec.hermes_agent_input_url = env::var("MOM_HERMES_AGENT_URL")
        .unwrap_or_else(|_| "github:NousResearch/hermes-agent".to_string());
    spec.labels
        .insert(LABEL_MANAGED.to_string(), "true".to_string());
    spec.labels.insert(
        LABEL_VERSION.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    spec.labels
        .insert("mom.workspace".to_string(), spec.workspace_name.clone());
    Ok(())
}

async fn generate_ssh_keypair(dir: &Path, name: &str) -> Result<String> {
    let private_key = dir.join("ssh_ed25519");
    let public_key = dir.join("ssh_ed25519.pub");
    if !private_key.exists() || !public_key.exists() {
        let status = TokioCommand::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                &format!("agentmom-microvm-{name}"),
                "-f",
                private_key
                    .to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 SSH key path {}", private_key.display()))?,
            ])
            .stdin(Stdio::null())
            .status()
            .await
            .with_context(|| format!("generate SSH key {}", private_key.display()))?;
        if !status.success() {
            bail!("ssh-keygen exited with {status}");
        }
    }
    let key = fs::read_to_string(&public_key)
        .with_context(|| format!("read SSH public key {}", public_key.display()))?;
    Ok(key.trim().to_string())
}

async fn ensure_ssh_host_keypair(dir: &Path, name: &str) -> Result<(String, String)> {
    let host_key_dir = dir.join("guest-ssh");
    fs::create_dir_all(&host_key_dir)
        .with_context(|| format!("create {}", host_key_dir.display()))?;
    let private_key = host_key_dir.join("ssh_host_ed25519_key");
    let public_key = host_key_dir.join("ssh_host_ed25519_key.pub");
    if !private_key.exists() || !public_key.exists() {
        let status = TokioCommand::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                &format!("agentmom-microvm-host-{name}"),
                "-f",
                private_key.to_str().ok_or_else(|| {
                    anyhow!("non-UTF-8 SSH host key path {}", private_key.display())
                })?,
            ])
            .stdin(Stdio::null())
            .status()
            .await
            .with_context(|| format!("generate SSH host key {}", private_key.display()))?;
        if !status.success() {
            bail!("ssh-keygen exited with {status}");
        }
    }
    let key = fs::read_to_string(&public_key)
        .with_context(|| format!("read SSH host public key {}", public_key.display()))?;
    Ok((key.trim().to_string(), host_key_dir.display().to_string()))
}

fn ensure_spec_ssh_host_identity(dir: &Path, name: &str, spec: &mut MicrovmSpec) -> Result<()> {
    if !spec.ssh_host_public_key.trim().is_empty() && !spec.ssh_host_key_dir.trim().is_empty() {
        return Ok(());
    }
    let host_key_dir = dir.join("guest-ssh");
    fs::create_dir_all(&host_key_dir)
        .with_context(|| format!("create {}", host_key_dir.display()))?;
    let private_key = host_key_dir.join("ssh_host_ed25519_key");
    let public_key = host_key_dir.join("ssh_host_ed25519_key.pub");
    if !private_key.exists() || !public_key.exists() {
        let status = std::process::Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                &format!("agentmom-microvm-host-{name}"),
                "-f",
                private_key.to_str().ok_or_else(|| {
                    anyhow!("non-UTF-8 SSH host key path {}", private_key.display())
                })?,
            ])
            .stdin(std::process::Stdio::null())
            .status()
            .with_context(|| format!("generate SSH host key {}", private_key.display()))?;
        if !status.success() {
            bail!("ssh-keygen exited with {status}");
        }
    }
    spec.ssh_host_public_key = fs::read_to_string(&public_key)
        .with_context(|| format!("read SSH host public key {}", public_key.display()))?
        .trim()
        .to_string();
    spec.ssh_host_key_dir = host_key_dir.display().to_string();
    Ok(())
}

fn validate_ssh_host_identity(spec: &MicrovmSpec) -> Result<()> {
    if spec.ssh_host_public_key.trim().is_empty() {
        bail!("microVM spec {} is missing ssh_host_public_key", spec.name);
    }
    if spec.ssh_host_key_dir.trim().is_empty() {
        bail!("microVM spec {} is missing ssh_host_key_dir", spec.name);
    }
    Ok(())
}

fn known_hosts_entry(spec: &MicrovmSpec) -> String {
    format!("{} {}\n", spec.guest_ip, spec.ssh_host_public_key.trim())
}

fn acquire_machine_state_lock() -> Result<MachineStateLock> {
    let lock_path = runtime_home()?.join(".machine-state.flock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    for _ in 0..100 {
        match file.try_lock_exclusive() {
            Ok(()) => {
                return Ok(MachineStateLock { file });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("lock {}", lock_path.display()));
            }
        }
    }
    bail!("timed out waiting for {}", lock_path.display())
}

struct MachineStateLock {
    file: fs::File,
}

impl Drop for MachineStateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn allocate_machine_index(name: &str) -> Result<u16> {
    let root = machines_dir()?;
    if !root.exists() {
        return Ok(MIN_MACHINE_INDEX);
    }

    let mut used = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let spec_path = entry.path().join("spec.json");
        if !spec_path.exists() {
            continue;
        }
        let bytes =
            fs::read(&spec_path).with_context(|| format!("read {}", spec_path.display()))?;
        let spec: MicrovmSpec = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", spec_path.display()))?;
        if spec.name == name {
            return Ok(spec.machine_index);
        }
        used.insert(spec.machine_index);
    }

    for index in MIN_MACHINE_INDEX..=MAX_MACHINE_INDEX {
        if !used.contains(&index) {
            return Ok(index);
        }
    }
    bail!(
        "no free microVM addresses remain in /24 range {}-{}",
        MIN_MACHINE_INDEX,
        MAX_MACHINE_INDEX
    )
}

fn microvm_cidr_prefix() -> String {
    if let Ok(cidr) = env::var("MOM_MICROVM_CIDR")
        && let Some((address, _prefix_len)) = cidr.split_once('/')
    {
        let octets: Vec<_> = address.split('.').collect();
        if octets.len() == 4 {
            return octets[..3].join(".");
        }
    }
    DEFAULT_MICROVM_CIDR_PREFIX.to_string()
}

fn microvm_flake_nix(spec: &MicrovmSpec) -> Result<String> {
    let system = microvm_system();
    Ok(format!(
        r#"{{
  description = "Agent Mom microvm.nix workspace {name}";

    inputs = {{
    nixpkgs.url = "{nixpkgs_url}";
    microvm = {{
      url = "{microvm_input_url}";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
    hermes-agent = {{
      url = "{hermes_agent_input_url}";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
  }};

  outputs = {{ self, nixpkgs, microvm, hermes-agent }}:
    let
      system = "{system}";
      spec = builtins.fromJSON (builtins.readFile ./spec.json);
      hermesAgentPackage = pkgs: import ./hermes-agent-package.nix {{
        inherit pkgs;
        inputs = {{ inherit hermes-agent; }};
      }};
    in {{
      packages.${{system}} = {{
        runner = self.nixosConfigurations.{name}.config.microvm.declaredRunner;
        default = self.packages.${{system}}.runner;
      }};
      nixosConfigurations.{name} = nixpkgs.lib.nixosSystem {{
        inherit system;
        modules = [
          microvm.nixosModules.microvm
          (import ./microvm-workspace.nix {{ inherit spec hermesAgentPackage; }})
        ];
      }};
    }};
}}
"#,
        name = spec.name,
        nixpkgs_url = spec.nixpkgs_url,
        microvm_input_url = spec.microvm_input_url,
        hermes_agent_input_url = spec.hermes_agent_input_url,
        system = system
    ))
}

fn microvm_system() -> String {
    env::var("MOM_MICROVM_SYSTEM").unwrap_or_else(|_| {
        match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64") => "x86_64-linux",
            ("linux", "aarch64") => "aarch64-linux",
            _ => "x86_64-linux",
        }
        .to_string()
    })
}

fn microvm_workspace_nix() -> &'static str {
    include_str!("../nix/microvm-workspace.nix")
}

fn microvm_systemd_template() -> String {
    env::var("MOM_MICROVM_SYSTEMD_TEMPLATE")
        .unwrap_or_else(|_| "agentmom-microvm@.service".to_string())
}

fn microvm_systemd_unit(name: &str) -> String {
    microvm_systemd_unit_from_template(&microvm_systemd_template(), name)
}

fn microvm_systemd_unit_from_template(template: &str, name: &str) -> String {
    if template.contains("@.") {
        template.replace("@.", &format!("@{name}."))
    } else {
        format!("{}@{name}.service", template.trim_end_matches(".service"))
    }
}

fn generated_flake_ref(dir: &Path, attr: &str) -> String {
    format!("path:{}#{attr}", dir.display())
}

fn hermes_agent_package_nix() -> &'static str {
    include_str!("../nix/hermes-agent-package.nix")
}

async fn ensure_microvm_host_ready(config: &MomConfig) -> Result<()> {
    for command in ["ip", "nix", "ssh", "ssh-keygen", "systemctl"] {
        if !command_exists(command).await {
            bail!("{command} is required for the microvm.nix runtime");
        }
    }
    if !cfg!(target_os = "linux") {
        bail!("the microvm.nix runtime requires a Linux host");
    }
    if !Path::new("/dev/kvm").exists() {
        bail!("/dev/kvm is required for the microvm.nix runtime");
    }
    let template = microvm_systemd_template();
    let template_description = format!("find systemd template {template}");
    require_success(
        TokioCommand::new("systemctl")
            .args(["cat", &template])
            .stdin(Stdio::null()),
        &template_description,
    )
    .await?;
    ensure_bridge_ready().await?;
    ensure_proxy_reachable(config).await?;
    ensure_probe_runner_builds(config).await?;
    Ok(())
}

async fn ensure_bridge_ready() -> Result<()> {
    let bridge = env::var("MOM_MICROVM_BRIDGE").unwrap_or_else(|_| "agentmom0".to_string());
    let prefix = microvm_cidr_prefix();
    let host_ip = env::var("MOM_MICROVM_HOST_IP").unwrap_or_else(|_| format!("{prefix}.1"));
    let output = TokioCommand::new("ip")
        .args(["-4", "addr", "show", "dev", &bridge])
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("inspect bridge {bridge}"))?;
    if !output.status.success() {
        bail!(
            "inspect bridge {bridge} exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains(&format!("{host_ip}/")) {
        bail!("bridge {bridge} does not have expected host address {host_ip}");
    }
    Ok(())
}

async fn ensure_proxy_reachable(config: &MomConfig) -> Result<()> {
    let Some(proxy_url) = config.credential_proxy_url() else {
        return Ok(());
    };
    let url = reqwest::Url::parse(proxy_url)
        .with_context(|| format!("parse credentials.proxy_url {proxy_url}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("credentials.proxy_url has no host: {proxy_url}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("credentials.proxy_url has no port: {proxy_url}"))?;
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .with_context(|| format!("connect to credential proxy {host}:{port} timed out"))?
    .with_context(|| format!("connect to credential proxy {host}:{port}"))?;
    Ok(())
}

async fn ensure_probe_runner_builds(config: &MomConfig) -> Result<()> {
    let dir = runtime_home()?.join("host-check");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let request = WorkspaceVmRequest {
        name: "agentmom-host-check".to_string(),
        replace: true,
        cpus: 1,
        memory_mib: 512,
        workspace_name: "host-check".to_string(),
        workspace_dir_name: "host-check".to_string(),
        workspace_quota_mib: 1,
    };
    let (ssh_host_public_key, ssh_host_key_dir) =
        ensure_ssh_host_keypair(&dir, &request.name).await?;
    let spec = microvm_spec(
        &request,
        config,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKf2probehostcheck agentmom-host-check".to_string(),
        ssh_host_public_key,
        ssh_host_key_dir,
    )?;
    fs::create_dir_all(&spec.workspace_dir)
        .with_context(|| format!("create host-check workspace dir {}", spec.workspace_dir))?;
    write_vm_definition(&dir, &spec, config)?;
    require_success(
        TokioCommand::new("nix")
            .args([
                "build",
                "--extra-experimental-features",
                "nix-command flakes",
                &generated_flake_ref(&dir, "runner"),
                "--no-link",
            ])
            .current_dir(&dir)
            .stdin(Stdio::null()),
        "build host-check microvm runner",
    )
    .await
}

async fn require_success(command: &mut TokioCommand, description: &str) -> Result<()> {
    let output = command
        .output()
        .await
        .with_context(|| description.to_string())?;
    if !output.status.success() {
        bail!(
            "{description} exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn cloud_hypervisor_control(name: &str, command: &str) -> Result<()> {
    if matches!(command, "pause" | "resume") {
        return cloud_hypervisor_control_http(name, command).await;
    }
    cloud_hypervisor_control_args(name, &[command]).await
}

async fn cloud_hypervisor_control_http(name: &str, command: &str) -> Result<()> {
    let stream = cloud_hypervisor_control_http_send(name, command).await?;
    read_cloud_hypervisor_control_http_response(name, command, stream).await
}

async fn cloud_hypervisor_control_http_send(
    name: &str,
    command: &str,
) -> Result<tokio::net::UnixStream> {
    let api_socket = machine_dir(name)?.join("control.socket");
    if !api_socket.exists() {
        bail!(
            "Cloud Hypervisor API socket for VM {name} is missing: {}",
            api_socket.display()
        );
    }
    let endpoint = match command {
        "pause" => "vm.pause",
        "resume" => "vm.resume",
        _ => bail!("unsupported direct Cloud Hypervisor command {command}"),
    };
    let mut stream = tokio::net::UnixStream::connect(&api_socket)
        .await
        .with_context(|| format!("connect Cloud Hypervisor API socket for VM {name}"))?;
    let request =
        format!("PUT /api/v1/{endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .with_context(|| format!("send Cloud Hypervisor {command} request for VM {name}"))?;
    Ok(stream)
}

async fn read_cloud_hypervisor_control_http_response(
    name: &str,
    command: &str,
    mut stream: tokio::net::UnixStream,
) -> Result<()> {
    let mut response = vec![0; 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .with_context(|| format!("Cloud Hypervisor {command} response timed out for VM {name}"))?
        .with_context(|| format!("read Cloud Hypervisor {command} response for VM {name}"))?;
    let response = String::from_utf8_lossy(&response[..read]);
    let status = response.lines().next().unwrap_or_default();
    if status.contains(" 2") {
        return Ok(());
    }
    bail!("Cloud Hypervisor {command} VM {name} failed: {status}");
}

async fn cloud_hypervisor_control_args(name: &str, args: &[&str]) -> Result<()> {
    let ch_remote = find_ch_remote(name)?;
    let api_socket = machine_dir(name)?.join("control.socket");
    if !api_socket.exists() {
        bail!(
            "Cloud Hypervisor API socket for VM {name} is missing: {}",
            api_socket.display()
        );
    }
    require_success(
        TokioCommand::new(ch_remote)
            .args(["--api-socket"])
            .arg(
                api_socket
                    .to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 API socket path {}", api_socket.display()))?,
            )
            .args(args)
            .stdin(Stdio::null()),
        &format!("cloud-hypervisor {} VM {name}", args.join(" ")),
    )
    .await
}

async fn wait_for_cloud_hypervisor_api(name: &str, timeout: Duration) -> Result<()> {
    let api_socket = machine_dir(name)?.join("control.socket");
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            bail!(
                "timed out waiting for Cloud Hypervisor API socket for VM {name}: {}",
                api_socket.display()
            );
        }
        if api_socket.exists() && tokio::net::UnixStream::connect(&api_socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn find_ch_remote(name: &str) -> Result<PathBuf> {
    let bin_dir = machine_dir(name)?.join("result/bin");
    let mut scripts = Vec::new();
    if bin_dir.exists() {
        for entry in
            fs::read_dir(&bin_dir).with_context(|| format!("read {}", bin_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                scripts.push(path);
            }
        }
    }
    for script in scripts {
        let Ok(contents) = fs::read_to_string(&script) else {
            continue;
        };
        if let Some(path) = extract_store_binary(&contents, "ch-remote")
            && path.exists()
        {
            return Ok(path);
        }
    }
    bail!("could not find ch-remote in VM {name} runner scripts; start the VM once to build result")
}

fn extract_store_binary(contents: &str, binary: &str) -> Option<PathBuf> {
    let needle = format!("/bin/{binary}");
    let mut offset = 0;
    while let Some(relative_end) = contents[offset..].find(&needle) {
        let end = offset + relative_end + needle.len();
        let start = contents[..end].rfind("/nix/store/")?;
        let path = &contents[start..end];
        if !path.chars().any(char::is_whitespace) {
            return Some(PathBuf::from(path));
        }
        offset = end;
    }
    None
}

async fn wait_for_systemd_active(name: &str, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if vm_status(name).await?.is_running() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for VM {name} systemd unit to become active");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_systemd_inactive(name: &str, timeout: Duration) -> Result<()> {
    let unit = microvm_systemd_unit(name);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let output = TokioCommand::new("systemctl")
            .args(["is-active", &unit])
            .stdin(Stdio::null())
            .output()
            .await;
        if let Ok(output) = output {
            let active = String::from_utf8_lossy(&output.stdout).trim().to_string();
            match active.as_str() {
                "active" | "activating" | "deactivating" => {}
                "inactive" | "failed" => return Ok(()),
                _ => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for VM {name} systemd unit to stop");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn microvm_unit_diagnostics(name: &str) -> String {
    let unit = microvm_systemd_unit(name);
    let mut status_cmd = TokioCommand::new("systemctl");
    status_cmd.args(["status", "--no-pager", "--full", &unit]);
    let status = diagnostic_command_output(status_cmd).await;

    let mut journal_cmd = TokioCommand::new("journalctl");
    journal_cmd.args(["-u", &unit, "-n", "80", "--no-pager"]);
    let journal = diagnostic_command_output(journal_cmd).await;

    format!("systemctl status {unit}\n{status}\n\njournalctl -u {unit} -n 80\n{journal}")
}

async fn diagnostic_command_output(mut command: TokioCommand) -> String {
    match command.stdin(Stdio::null()).output().await {
        Ok(output) => format!(
            "status: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("failed to run diagnostic command: {error}"),
    }
}

async fn wait_for_ssh(vm: &GuestVm, spec: &MicrovmSpec, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let output = TokioCommand::new("ssh")
            .args(ssh_common_args(vm.name(), spec)?)
            .arg(ssh_destination(spec))
            .arg("true")
            .stdin(Stdio::null())
            .output()
            .await;
        match output {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                if std::time::Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for SSH in VM {}: {}",
                        vm.name(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
            }
            Err(error) => {
                if std::time::Instant::now() >= deadline {
                    return Err(error).with_context(|| format!("wait for SSH in VM {}", vm.name()));
                }
            }
        }
        let delay = if started.elapsed() < Duration::from_secs(2) {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(200)
        };
        tokio::time::sleep(delay).await;
    }
}

async fn wait_for_vm_readiness(
    vm: &GuestVm,
    spec: &MicrovmSpec,
    timeout: Duration,
    readiness: VmReadiness,
    timing: &mut VmStartTiming,
) -> Result<()> {
    let started = Instant::now();
    wait_for_tcp_port(&spec.guest_ip, 22, timeout).await?;
    timing.tcp_22_ready_ms = Some(started.elapsed().as_millis());
    if readiness == VmReadiness::Ssh {
        wait_for_ssh(vm, spec, timeout).await?;
        timing.ssh_ready_ms = Some(started.elapsed().as_millis());
    }
    Ok(())
}

async fn prime_guest_network(spec: &MicrovmSpec) {
    let _ = TokioCommand::new("ip")
        .args([
            "neigh",
            "replace",
            &spec.guest_ip,
            "lladdr",
            &spec.mac,
            "dev",
            &spec.host_bridge,
            "nud",
            "reachable",
        ])
        .stdin(Stdio::null())
        .status()
        .await;
    let _ = TokioCommand::new("bridge")
        .args([
            "fdb", "replace", &spec.mac, "dev", &spec.tap, "master", "static",
        ])
        .stdin(Stdio::null())
        .status()
        .await;
}

async fn wait_for_tcp_port(host: &str, port: u16, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let connect = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await;
        match connect {
            Ok(Ok(_stream)) => return Ok(()),
            Ok(Err(error)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(error).with_context(|| format!("wait for TCP {host}:{port}"));
                }
            }
            Err(_elapsed) => {
                if std::time::Instant::now() >= deadline {
                    bail!("timed out waiting for TCP {host}:{port}");
                }
            }
        }
        let delay = if started.elapsed() < Duration::from_secs(2) {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(200)
        };
        tokio::time::sleep(delay).await;
    }
}

async fn run_ssh_shell(
    name: &str,
    spec: &MicrovmSpec,
    script: &str,
    stdin_bytes: Option<&[u8]>,
) -> Result<GuestOutput> {
    let script = logout_safe_shell_script(script);
    let mut child = TokioCommand::new("ssh")
        .args(ssh_common_args(name, spec)?)
        .arg(ssh_destination(spec))
        .arg(format!("/bin/sh -lc {}", shell_quote(&script)))
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run SSH shell in VM {name}"))?;
    if let Some(bytes) = stdin_bytes {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("SSH stdin unavailable for VM {name}"))?;
        stdin.write_all(bytes).await?;
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("wait for SSH shell in VM {name}"))?;
    Ok(GuestOutput {
        ok: output.status.success(),
        code: output.status.code().unwrap_or_default(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn logout_safe_shell_script(script: &str) -> String {
    format!(
        "\
(
{script}
)
__agentmom_status=$?
set +u
exit \"$__agentmom_status\"
"
    )
}

fn ssh_common_args(name: &str, _spec: &MicrovmSpec) -> Result<Vec<String>> {
    let dir = machine_dir(name)?;
    let private_key = dir.join("ssh_ed25519");
    if !private_key.exists() {
        bail!("missing SSH private key {}", private_key.display());
    }
    let known_hosts = dir.join("known_hosts");
    if !known_hosts.exists() {
        bail!("missing SSH known_hosts {}", known_hosts.display());
    }
    Ok(vec![
        "-F".to_string(),
        "/dev/null".to_string(),
        "-i".to_string(),
        private_key.display().to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=3".to_string(),
        "-o".to_string(),
        "GlobalKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
    ])
}

fn ssh_destination(spec: &MicrovmSpec) -> String {
    format!("root@{}", spec.guest_ip)
}

async fn systemctl(args: &[&str]) -> Result<()> {
    if !command_exists("systemctl").await {
        bail!("systemctl is required for the microvm.nix runtime");
    }
    let mut command = if runtime_sudo_enabled() {
        let mut command = TokioCommand::new("sudo");
        command.args(["-n", "systemctl"]);
        command
    } else {
        TokioCommand::new("systemctl")
    };
    let status = command.args(args).stdin(Stdio::null()).status().await?;
    if !status.success() {
        bail!("systemctl {} exited with {status}", args.join(" "));
    }
    Ok(())
}

async fn remove_runtime_dir_all(path: &std::path::Path) -> Result<()> {
    if runtime_sudo_enabled() {
        let status = TokioCommand::new("sudo")
            .args(["-n", "rm", "-rf", "--"])
            .arg(path)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("sudo rm -rf {} exited with {status}", path.display());
        }
        return Ok(());
    }
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
}

fn runtime_sudo_enabled() -> bool {
    env::var("MOM_RUNTIME_SUDO").ok().as_deref() == Some("1")
        || env::var("MOM_SYSTEMCTL_SUDO").ok().as_deref() == Some("1")
}

async fn command_exists(name: &str) -> bool {
    TokioCommand::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", shell_quote(name)))
        .stdin(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec(workspace_dir_name: String, workspace_dir: &Path) -> MicrovmSpec {
        MicrovmSpec {
            name: "mom-test".to_string(),
            workspace_name: "test".to_string(),
            workspace_dir_name,
            cpus: 1,
            memory_mib: 512,
            workspace_quota_mib: 1024,
            machine_index: MIN_MACHINE_INDEX,
            guest_ip: "192.168.83.10".to_string(),
            host_ip: "192.168.83.1".to_string(),
            host_bridge: "agentmom0".to_string(),
            tap: "amvm10".to_string(),
            mac: "02:00:00:83:00:0a".to_string(),
            workspace_dir: workspace_dir.display().to_string(),
            hermes_profile: "main".to_string(),
            hermes_model: "gpt-5.5".to_string(),
            credential_proxy_url: Some("http://192.168.83.1:1080".to_string()),
            credential_proxy_ca_file: Some("agentmom-proxy.crt".to_string()),
            nixpkgs_url: "path:/nix/store/nixpkgs-source".to_string(),
            microvm_input_url: "path:/nix/store/microvm-source".to_string(),
            hermes_agent_input_url: "path:/nix/store/hermes-agent-source".to_string(),
            ssh_public_key: "ssh-ed25519 test".to_string(),
            ssh_host_public_key: "ssh-ed25519 host-test".to_string(),
            ssh_host_key_dir: workspace_dir.display().to_string(),
            labels: HashMap::new(),
        }
    }

    #[test]
    fn generated_file_write_replaces_content_atomically() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("spec.json");

        write_file_if_changed(&path, b"old")?;
        write_file_if_changed(&path, b"new")?;

        assert_eq!(fs::read(&path)?, b"new");
        let leftover_temp_files = fs::read_dir(dir.path())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".spec.json.tmp.")
            })
            .count();
        assert_eq!(leftover_temp_files, 0);
        Ok(())
    }

    #[test]
    fn stale_generated_file_removal_is_idempotent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agentmom-proxy.crt");

        write_file_if_changed(&path, b"cert")?;
        remove_file_if_exists(&path)?;
        remove_file_if_exists(&path)?;

        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn active_runner_with_stopped_state_is_not_ready() {
        assert_eq!(
            vm_status_from_systemd("active", "stopped"),
            VmStatus::Stopped
        );
        assert_eq!(
            vm_status_from_systemd("activating", "stopped"),
            VmStatus::Stopped
        );
        assert_eq!(
            vm_status_from_systemd("active", "running"),
            VmStatus::Running
        );
        assert_eq!(
            vm_status_from_systemd("active", "suspended"),
            VmStatus::Draining
        );
    }

    #[test]
    fn workspace_source_validation_refuses_changed_source_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let current = dir.path().join("current");
        let stored = dir.path().join("stored");
        fs::create_dir_all(&current)?;
        fs::create_dir_all(&stored)?;
        let spec = test_spec(current.display().to_string(), &stored);

        let error = validate_workspace_source("mom-test", &spec).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("refusing to rewrite workspace source for VM mom-test")
        );
        Ok(())
    }

    #[test]
    fn workspace_source_validation_refuses_missing_source() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("missing");
        let spec = test_spec(missing.display().to_string(), &missing);

        let error = validate_workspace_source("mom-test", &spec).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("workspace source for VM mom-test is missing")
        );
        Ok(())
    }

    #[test]
    fn workspace_source_validation_refuses_file_source() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("workspace-file");
        fs::write(&file, b"not a directory")?;
        let spec = test_spec(file.display().to_string(), &file);

        let error = validate_workspace_source("mom-test", &spec).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("workspace source for VM mom-test is not a directory")
        );
        Ok(())
    }

    #[test]
    fn microvm_template_exports_proxy_ca_env_for_hermes() {
        let template = microvm_workspace_nix();

        assert!(template.contains("export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt"));
        assert!(template.contains("export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt"));
        assert!(template.contains("REQUESTS_CA_BUNDLE: /etc/ssl/certs/ca-certificates.crt"));
        assert!(template.contains("SSL_CERT_FILE: /etc/ssl/certs/ca-certificates.crt"));
    }

    #[test]
    fn guest_command_script_prefers_launcher_and_has_profile_fallback() -> Result<()> {
        let script = guest_command_script(&["printf".to_string(), "hello world".to_string()])?;

        assert!(script.contains("/run/current-system/sw/bin/agentmom-run"));
        assert!(script.contains("set -e"));
        assert!(script.contains(" -- "));
        assert!(script.contains("printf"));
        assert!(script.contains("'hello world'"));
        assert!(script.contains(". /etc/profile.d/mom.sh"));
        assert!(script.contains(". /etc/profile.d/agentmom-proxy.sh"));
        assert!(script.contains("cd /workspace"));
        assert!(guest_command_script(&[]).is_err());
        Ok(())
    }

    #[test]
    fn guest_command_script_falls_back_when_hermes_wrapper_is_missing() -> Result<()> {
        let script =
            guest_command_script(&[GUEST_AGENTMOM_HERMES.to_string(), "--help".to_string()])?;

        assert!(script.contains(GUEST_AGENTMOM_HERMES));
        assert!(
            script.contains("exec '/run/current-system/sw/bin/agentmom-run' -- 'hermes' '--help'")
        );
        assert!(script.contains("exec 'hermes' '--help'"));
        Ok(())
    }

    #[test]
    fn ssh_shell_wrapper_keeps_nounset_out_of_logout() {
        let script = logout_safe_shell_script("set -eu\necho ok");

        assert!(script.starts_with("(\nset -eu\necho ok\n)"));
        assert!(script.contains("__agentmom_status=$?"));
        assert!(script.contains("set +u\nexit \"$__agentmom_status\""));
    }

    #[test]
    fn microvm_template_installs_agentmom_run_and_uses_hermes_model_schema() {
        let template = microvm_workspace_nix();

        assert!(template.contains("export MOM_WORKSPACE_NAME=${spec.workspace_name}"));
        assert!(template.contains("writeShellScriptBin \"agentmom-run\""));
        assert!(template.contains("writeShellScriptBin \"agentmom-hermes\""));
        assert!(template.contains("writeShellScriptBin \"agentmom-hermes-acp\""));
        assert!(template.contains("writeShellScriptBin \"agentmom-hermes-dashboard\""));
        assert!(template.contains("writeShellScriptBin \"agentmom-hermes-dashboard-start\""));
        assert!(template.contains("systemd.services.agentmom-hermes-dashboard"));
        assert!(template.contains("HERMES_DASHBOARD_SESSION_TOKEN=agentmom-dashboard"));
        assert!(template.contains("X-Hermes-Session-Token: agentmom-dashboard"));
        assert!(
            template.contains(
                "ExecStart = \"${agentmomHermesDashboard}/bin/agentmom-hermes-dashboard\""
            )
        );
        assert!(template.contains("Restart = \"on-failure\""));
        assert!(template.contains("Hermes web_dist is missing"));
        assert!(template.contains("HERMES_DASHBOARD_SESSION_TOKEN = \"agentmom-dashboard\""));
        assert!(template.contains(". /etc/profile.d/mom.sh"));
        assert!(template.contains(". /etc/profile.d/agentmom-proxy.sh"));
        assert!(!template.contains("set -eu"));
        assert!(!template.contains("setsid hermes dashboard"));
        assert!(
            template.contains(
                "model:\n      provider: openrouter\n      default: ${spec.hermes_model}"
            )
        );
        assert!(!template.contains("openai-codex"));
        assert!(!template.contains("default_provider:"));
        assert!(template.contains("systemd.services.sshd-keygen.enable = lib.mkForce false;"));
        assert!(template.contains("PerSourcePenalties = \"no\";"));
        assert!(template.contains("environment.etc.\"ssh/ssh_host_ed25519_key\""));
        assert!(template.contains("builtins.readFile ./guest-ssh/ssh_host_ed25519_key"));
        assert!(template.contains("mode = \"0600\";"));
        assert!(!template.contains("agentmom-ssh-host-key"));
        assert!(!template.contains("after = [ \"sshd-keygen.service\" ];"));
        assert!(!template.contains("requires = [ \"sshd-keygen.service\" ];"));
    }

    #[test]
    fn microvm_template_tells_agents_to_register_previews() {
        let template = microvm_workspace_nix();

        assert!(template.contains("report the preview target to Agent Mom"));
        assert!(template.contains(
            "mom workspace preview register \"$MOM_WORKSPACE_NAME\" --preview web --port <port>"
        ));
        assert!(template.contains("This is a host-side Agent Mom command"));
    }

    #[test]
    fn microvm_systemd_unit_supports_dev_template() {
        assert_eq!(
            microvm_systemd_unit_from_template("agentmom-microvm@.service", "mom-dev-preview"),
            "agentmom-microvm@mom-dev-preview.service"
        );
        assert_eq!(
            microvm_systemd_unit_from_template("agentmom-dev-microvm@.service", "mom-dev-preview"),
            "agentmom-dev-microvm@mom-dev-preview.service"
        );
        assert_eq!(
            microvm_systemd_unit_from_template("agentmom-dev-microvm", "mom-dev-preview"),
            "agentmom-dev-microvm@mom-dev-preview.service"
        );
    }

    #[test]
    fn generated_flake_ref_forces_plain_path_source() {
        assert_eq!(
            generated_flake_ref(
                Path::new("/tmp/agentmom/.state/microvms/host-check"),
                "runner"
            ),
            "path:/tmp/agentmom/.state/microvms/host-check#runner"
        );
    }

    #[test]
    fn microvm_template_caps_guest_hostname_length() {
        let template = microvm_workspace_nix();

        assert!(template.contains("guestHostName ="));
        assert!(template.contains("builtins.stringLength spec.name > 63"));
        assert!(template.contains("\"${builtins.substring 0 62 spec.name}x\""));
        assert!(template.contains("networking.hostName = guestHostName;"));
    }
}
