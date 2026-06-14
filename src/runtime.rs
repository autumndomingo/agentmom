use std::{collections::HashMap, io::Write};

use tokio::io::AsyncWriteExt;

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
    Crashed,
    Missing,
    Unknown,
}

impl VmStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Paused => "paused",
            Self::Crashed => "crashed",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Draining)
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

    pub(crate) async fn write_file(&self, path: &str, bytes: &[u8], mode: u32) -> Result<()> {
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(self, &spec, Duration::from_secs(90)).await?;
        let parent = guest_parent_dir(path)?;
        let script = format!(
            "set -eu\ninstall -d -m 0755 {parent}\ncat > {path}\nchmod {mode:o} {path}\n",
            parent = shell_quote(&parent),
            path = shell_quote(path),
            mode = mode
        );
        let output = run_ssh_shell(&self.name, &spec, &script, Some(bytes)).await?;
        if !output.ok {
            bail!(
                "write {} in VM {} exited with {}\n{}",
                path,
                self.name,
                output.code,
                output.stderr
            );
        }
        Ok(())
    }

    pub(crate) async fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let script = format!("install -d -m {mode:o} {}", shell_quote(path), mode = mode);
        let output = self.shell(&script).await?;
        if !output.ok {
            bail!(
                "mkdir {} in VM {} exited with {}\n{}",
                path,
                self.name,
                output.code,
                output.stderr
            );
        }
        Ok(())
    }

    pub(crate) async fn spawn_shell(&self, script: &str) -> Result<tokio::process::Child> {
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(self, &spec, Duration::from_secs(90)).await?;
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

    pub(crate) async fn forward_tcp(
        &self,
        bind_host: &str,
        host_port: u16,
        guest_port: u16,
    ) -> Result<tokio::process::Child> {
        let spec = load_microvm_spec(&self.name)?;
        wait_for_ssh(self, &spec, Duration::from_secs(90)).await?;
        let mut command = TokioCommand::new("ssh");
        command
            .args(ssh_common_args(&self.name, &spec)?)
            .args([
                "-N",
                "-L",
                &format!("{bind_host}:{host_port}:127.0.0.1:{guest_port}"),
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
    credential_mode: String,
    credential_proxy_url: Option<String>,
    credential_proxy_ca_file: Option<String>,
    nixpkgs_url: String,
    microvm_input_url: String,
    ssh_public_key: String,
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
    let spec = microvm_spec(&request, &config, ssh_public_key)?;
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
    ensure_machine_exists(name)?;
    if vm_status(name).await?.is_running() {
        let vm = GuestVm::new(name);
        let spec = load_microvm_spec(name)?;
        wait_for_ssh(&vm, &spec, Duration::from_secs(90)).await?;
        return Ok(vm);
    }
    refresh_vm_definition(name)?;
    systemctl(&["start", &format!("agentmom-microvm@{name}.service")]).await?;
    let vm = GuestVm::new(name);
    let spec = load_microvm_spec(name)?;
    if let Err(error) = wait_for_systemd_active(name, FRESH_VM_SYSTEMD_ACTIVE_TIMEOUT).await {
        let diagnostics = microvm_unit_diagnostics(name).await;
        return Err(error.context(format!(
            "microVM unit diagnostics for {name}:\n{diagnostics}"
        )));
    }
    if let Err(error) = wait_for_ssh(&vm, &spec, FRESH_VM_SSH_READY_TIMEOUT).await {
        let diagnostics = microvm_unit_diagnostics(name).await;
        return Err(error.context(format!(
            "microVM unit diagnostics for {name}:\n{diagnostics}"
        )));
    }
    fs::write(machine_dir(name)?.join("state"), b"running\n")?;
    Ok(vm)
}

pub(crate) async fn stop_vm(name: &str) -> Result<()> {
    stop_vm_with_timeout(name, Duration::from_secs(20)).await
}

pub(crate) async fn stop_vm_with_timeout(name: &str, timeout: Duration) -> Result<()> {
    ensure_machine_exists(name)?;
    let unit = format!("agentmom-microvm@{name}.service");
    systemctl(&["stop", "--job-mode=replace", &unit]).await?;
    wait_for_systemd_inactive(name, timeout).await?;
    fs::write(machine_dir(name)?.join("state"), b"stopped\n")?;
    Ok(())
}

pub(crate) async fn remove_vm(name: &str) -> Result<()> {
    if machine_dir(name)?.exists() {
        stop_vm(name).await?;
    }
    let dir = machine_dir(name)?;
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(())
}

pub(crate) async fn remove_workspace_dir(workspace_dir_name: &str) -> Result<()> {
    let path = workspace_dir_path(workspace_dir_name)?;
    if path.exists() {
        fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

pub(crate) async fn vm_status(name: &str) -> Result<VmStatus> {
    if !machine_dir(name)?.exists() {
        return Ok(VmStatus::Missing);
    }
    if command_exists("systemctl").await {
        let unit = format!("agentmom-microvm@{name}.service");
        let output = TokioCommand::new("systemctl")
            .args(["is-active", &unit])
            .stdin(Stdio::null())
            .output()
            .await;
        if let Ok(output) = output {
            let active = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Ok(match active.as_str() {
                "active" | "activating" => VmStatus::Running,
                "deactivating" => VmStatus::Draining,
                "failed" => VmStatus::Crashed,
                "inactive" => VmStatus::Stopped,
                _ => VmStatus::Unknown,
            });
        }
    }
    let state = fs::read_to_string(machine_dir(name)?.join("state")).unwrap_or_default();
    Ok(match state.trim() {
        "running" => VmStatus::Running,
        "stopped" => VmStatus::Stopped,
        "paused" => VmStatus::Paused,
        "crashed" => VmStatus::Crashed,
        _ => VmStatus::Unknown,
    })
}

pub(crate) async fn apply_guest_auth_config(vm: &GuestVm, config: &MomConfig) -> Result<()> {
    println!("writing VM auth/config from host config");
    config.validate_for_guest_config()?;
    let hermes_home = format!("{GUEST_HERMES_HOME}/{}", config.hermes_profile());

    vm.mkdir("/workspace", 0o755).await?;
    vm.mkdir(GUEST_HERMES_HOME, 0o700).await?;
    vm.mkdir(&hermes_home, 0o700).await?;
    vm.mkdir(&format!("{hermes_home}/home"), 0o700).await?;
    vm.write_file(
        &format!("{hermes_home}/config.yaml"),
        hermes_config_yaml(config).as_bytes(),
        0o600,
    )
    .await?;
    vm.write_file(
        &format!("{hermes_home}/SOUL.md"),
        hermes_soul_md().as_bytes(),
        0o600,
    )
    .await?;
    if let Some(proxy_url) = config.credential_proxy_url() {
        vm.write_file(
            "/etc/profile.d/agentmom-proxy.sh",
            proxy_env_sh(proxy_url).as_bytes(),
            0o644,
        )
        .await?;
    }
    if let Some(ca_path) = &config.credentials.proxy_ca_path {
        let ca_path = resolve_required_file(ca_path, "credentials.proxy_ca_path")?;
        let ca = fs::read(&ca_path).with_context(|| format!("read {}", ca_path.display()))?;
        vm.write_file(
            "/usr/local/share/ca-certificates/agentmom-proxy.crt",
            &ca,
            0o644,
        )
        .await?;
    }

    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        vm,
        &format!(
            r#"
set -eu
rm -f /root/.hermes/auth.json /root/.hermes-agent/*/auth.json
chmod 700 /root/.hermes-agent {hermes_home_q}
chmod 600 {hermes_home_q}/config.yaml {hermes_home_q}/SOUL.md
if [ -f /usr/local/share/ca-certificates/agentmom-proxy.crt ]; then update-ca-certificates || true; fi
ln -sfn {hermes_home_q} /root/.hermes
sync
"#
        ),
    )
    .await
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
set -eu
if [ -f /root/.hermes/auth.json ]; then
  echo "raw auth files are present in the VM" >&2
  exit 1
fi
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
    let command = command
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let output = vm.shell(&format!("exec {command}")).await?;
    Ok(json!({
        "ok": output.ok,
        "code": output.code,
        "stdout": output.stdout,
        "stderr": output.stderr
    }))
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
    apply_current_microvm_config(&mut spec, &config)?;
    write_vm_definition(&dir, &spec, &config)
}

fn write_vm_definition(dir: &Path, spec: &MicrovmSpec, config: &MomConfig) -> Result<()> {
    if let Some(ca_path) = &config.credentials.proxy_ca_path {
        sync_proxy_ca_file(dir, ca_path)?;
    }
    write_file_if_changed(&dir.join("spec.json"), &serde_json::to_vec_pretty(spec)?)?;
    write_file_if_changed(
        dir.join("microvm-workspace.nix").as_path(),
        microvm_workspace_nix().as_bytes(),
    )?;
    write_file_if_changed(&dir.join("flake.nix"), microvm_flake_nix(spec)?.as_bytes())?;
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
        credential_mode: String::new(),
        credential_proxy_url: None,
        credential_proxy_ca_file: None,
        nixpkgs_url: String::new(),
        microvm_input_url: String::new(),
        ssh_public_key,
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
    spec.credential_mode = if config.credential_proxy_url().is_some() {
        "openrouter-proxy".to_string()
    } else {
        "openai-codex".to_string()
    };
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

fn acquire_machine_state_lock() -> Result<MachineStateLock> {
    let lock_dir = runtime_home()?.join(".machine-state.lock");
    for _ in 0..100 {
        match fs::create_dir(&lock_dir) {
            Ok(()) => {
                return Ok(MachineStateLock { path: lock_dir });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", lock_dir.display()));
            }
        }
    }
    bail!("timed out waiting for {}", lock_dir.display())
}

struct MachineStateLock {
    path: PathBuf,
}

impl Drop for MachineStateLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
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
    let system = env::var("MOM_MICROVM_SYSTEM").unwrap_or_else(|_| "x86_64-linux".to_string());
    Ok(format!(
        r#"{{
  description = "Agent Mom microvm.nix workspace {name}";

  inputs = {{
    nixpkgs.url = "{nixpkgs_url}";
    microvm = {{
      url = "{microvm_input_url}";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
  }};

  outputs = {{ self, nixpkgs, microvm }}:
    let
      system = "{system}";
      spec = builtins.fromJSON (builtins.readFile ./spec.json);
    in {{
      packages.${{system}} = {{
        runner = self.nixosConfigurations.{name}.config.microvm.declaredRunner;
        default = self.packages.${{system}}.runner;
      }};
      nixosConfigurations.{name} = nixpkgs.lib.nixosSystem {{
        inherit system;
        modules = [
          microvm.nixosModules.microvm
          (import ./microvm-workspace.nix {{ inherit spec; }})
        ];
      }};
    }};
}}
"#,
        name = spec.name,
        nixpkgs_url = spec.nixpkgs_url,
        microvm_input_url = spec.microvm_input_url,
        system = system
    ))
}

fn microvm_workspace_nix() -> &'static str {
    include_str!("../nix/microvm-workspace.nix")
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
    require_success(
        TokioCommand::new("systemctl")
            .args(["cat", "agentmom-microvm@.service"])
            .stdin(Stdio::null()),
        "find systemd template agentmom-microvm@.service",
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
    let spec = microvm_spec(
        &request,
        config,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKf2probehostcheck agentmom-host-check".to_string(),
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
                ".#runner",
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
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !vm_status(name).await?.is_running() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for VM {name} systemd unit to stop");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn microvm_unit_diagnostics(name: &str) -> String {
    let unit = format!("agentmom-microvm@{name}.service");
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn run_ssh_shell(
    name: &str,
    spec: &MicrovmSpec,
    script: &str,
    stdin_bytes: Option<&[u8]>,
) -> Result<GuestOutput> {
    let mut child = TokioCommand::new("ssh")
        .args(ssh_common_args(name, spec)?)
        .arg(ssh_destination(spec))
        .arg(format!("/bin/sh -lc {}", shell_quote(script)))
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

fn ssh_common_args(name: &str, _spec: &MicrovmSpec) -> Result<Vec<String>> {
    let private_key = machine_dir(name)?.join("ssh_ed25519");
    if !private_key.exists() {
        bail!("missing SSH private key {}", private_key.display());
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
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
    ])
}

fn ssh_destination(spec: &MicrovmSpec) -> String {
    format!("root@{}", spec.guest_ip)
}

fn guest_parent_dir(path: &str) -> Result<String> {
    path.rsplit_once('/')
        .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("guest path has no parent: {path}"))
}

async fn systemctl(args: &[&str]) -> Result<()> {
    if !command_exists("systemctl").await {
        bail!("systemctl is required for the microvm.nix runtime");
    }
    let status = TokioCommand::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("systemctl {} exited with {status}", args.join(" "));
    }
    Ok(())
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
}
