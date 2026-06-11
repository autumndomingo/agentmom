use std::{
    convert::Infallible,
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use clap::{Args, Parser, Subcommand};
use microsandbox::{
    MicrosandboxError, Sandbox, Snapshot, SnapshotDestination, sandbox::SandboxStatus,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command as TokioCommand;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

const IMAGE: &str = "alpine";
const LABEL_MANAGED: &str = "mom.managed";
const LABEL_VERSION: &str = "mom.version";
const GUEST_CODEX_HOME: &str = "/root/.codex";
const GUEST_HERMES_HOME: &str = "/root/.hermes-agent";
const GUEST_OPENCODE_DATA_HOME: &str = "/root/.local/share/opencode";
const GUEST_OPENCODE_CONFIG_HOME: &str = "/root/.config/opencode";
const OPENCODE_GUEST_PORT: u16 = 4096;
const BASE_BUILDER_NAME: &str = "mom-base-builder";

#[derive(Debug, Parser)]
#[command(
    name = "mom",
    about = "Agent Mom: small VM manager for Alpine microsandbox agent boxes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create and provision a new Alpine VM.
    Create(CreateArgs),
    /// List Agent Mom-managed VMs.
    List {
        /// Include sandboxes not created by Agent Mom.
        #[arg(long)]
        all: bool,
    },
    /// Start a stopped VM in the background.
    Start { name: String },
    /// Stop a VM.
    Stop { name: String },
    /// Remove a VM, stopping it first if needed.
    Rm {
        name: Option<String>,
        /// Remove all Agent Mom-managed VMs.
        #[arg(long)]
        all: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        force: bool,
    },
    /// Run a command in a VM and print captured output.
    Exec {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Open an interactive shell in a VM.
    Enter { name: String },
    /// Run Codex inside a VM.
    Codex {
        name: String,
        #[arg(required = true)]
        prompt: Vec<String>,
    },
    /// Run Hermes inside a VM.
    Hermes {
        name: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run basic tool checks inside a VM.
    Doctor { name: String },
    /// Manage durable user workspaces backed by named volumes.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Inspect this worker node.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Run the central HTTP API and SSE notification service.
    Api(ApiArgs),
    /// Run a worker that claims jobs from a central API.
    Worker(WorkerArgs),
    /// Run the single-host scheduler and backup worker.
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
struct CreateArgs {
    name: String,
    /// Replace an existing VM with this name.
    #[arg(long)]
    replace: bool,
    /// vCPUs to allocate.
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    /// Memory in MiB.
    #[arg(long, default_value_t = 2048)]
    memory: u64,
    /// Rebuild the base snapshot before creating the VM.
    #[arg(long)]
    rebuild_snapshot: bool,
    /// Provision directly from Alpine instead of the base snapshot.
    #[arg(long)]
    no_snapshot: bool,
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Create a durable workspace and its VM.
    Create(WorkspaceCreateArgs),
    /// List durable workspaces.
    List,
    /// Show workspace status, runtime, backup, and recent event summary.
    Inspect { name: String },
    /// Show workspace event trail.
    Events {
        name: String,
        /// Include events since now minus this duration, e.g. 30m, 2h, 1d.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Mark a workspace desired-running and start it.
    Start { name: String },
    /// Mark a workspace desired-stopped and stop it.
    Stop { name: String },
    /// Run a command in a workspace VM and update its activity timestamp.
    Exec {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run Codex in a workspace VM and update its activity timestamp.
    Codex {
        name: String,
        #[arg(required = true)]
        prompt: Vec<String>,
    },
    /// Run Hermes in a workspace VM and update its activity timestamp.
    Hermes {
        name: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Re-apply Codex/Hermes/proxy configuration to an existing workspace VM.
    RefreshConfig { name: String },
    /// Verify proxy-mode credentials and egress in a workspace VM.
    ProxySmoke { name: String },
    /// Back up a workspace volume now.
    Backup {
        name: String,
        /// Leave a running workspace stopped after backup.
        #[arg(long)]
        leave_stopped: bool,
    },
    /// List recorded backup artifacts for a workspace.
    Backups { name: String },
    /// Restore a workspace from a local tar backup artifact.
    Restore {
        name: String,
        /// Backup artifact ID. Defaults to the latest local tar backup.
        #[arg(long)]
        backup_id: Option<String>,
    },
    /// Remove a workspace record and VM. The named volume is kept by default.
    Rm {
        name: String,
        /// Remove the workspace named volume too.
        #[arg(long)]
        volume: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum NodeCommand {
    /// Show local worker node status.
    Status,
}

#[derive(Debug, Args)]
struct WorkspaceCreateArgs {
    name: String,
    /// User or account identifier that owns this workspace.
    #[arg(long)]
    user: Option<String>,
    /// Replace an existing VM with this name.
    #[arg(long)]
    replace: bool,
    /// vCPUs to allocate.
    #[arg(long, default_value_t = 1)]
    cpus: u8,
    /// Memory in MiB.
    #[arg(long, default_value_t = 2048)]
    memory: u64,
    /// Workspace volume quota in MiB.
    #[arg(long, default_value_t = 10240)]
    volume_quota: u32,
    /// Auto-stop after this many idle seconds.
    #[arg(long, default_value_t = 1800)]
    idle_timeout: u64,
    /// Back up at most this often. Set 0 to disable daemon backups.
    #[arg(long, default_value_t = 900)]
    backup_interval: u64,
    /// Rebuild the base snapshot before creating the VM.
    #[arg(long)]
    rebuild_snapshot: bool,
    /// Provision directly from Alpine instead of the base snapshot.
    #[arg(long)]
    no_snapshot: bool,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    /// Scheduler loop interval in seconds.
    #[arg(long, default_value_t = 30)]
    interval: u64,
    /// Run one reconciliation pass and exit.
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Args)]
struct ApiArgs {
    /// HTTP bind address for the API.
    #[arg(long, default_value = "127.0.0.1:8080", env = "MOM_API_BIND")]
    bind: String,
}

#[derive(Debug, Args)]
struct WorkerArgs {
    /// Central Agent Mom API URL, e.g. http://127.0.0.1:8080.
    #[arg(long, env = "MOM_API_URL")]
    api_url: String,
    /// Fallback polling interval in seconds.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Claim and run at most one job, then exit.
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Clone)]
struct WorkspaceMount {
    volume_name: String,
    volume_quota_mib: u32,
    workspace_name: String,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceRecord {
    name: String,
    user_id: String,
    sandbox_name: String,
    volume_name: String,
    desired_state: String,
    status: String,
    cpus: u8,
    memory_mib: u32,
    volume_quota_mib: u32,
    idle_timeout_secs: u64,
    backup_interval_secs: u64,
    last_used_at: i64,
    last_backup_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct WorkspaceEvent {
    id: i64,
    workspace_name: String,
    node_id: String,
    event_type: String,
    status: String,
    message: String,
    metadata_json: String,
    created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
struct BackupRecord {
    id: String,
    workspace_name: String,
    node_id: String,
    kind: String,
    location: String,
    status: String,
    size_bytes: Option<i64>,
    created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeCapacity {
    cpus: u32,
    memory_mib: u64,
    max_active_workspaces: u32,
    disk_reserve_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodePressure {
    active_workspaces: usize,
    running_sandboxes: usize,
    managed_sandboxes: usize,
    allocated_memory_mib: u64,
    disk_available_mib: Option<u64>,
    capacity_ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRecord {
    id: String,
    workspace_name: String,
    node_id: Option<String>,
    kind: String,
    status: String,
    payload_json: String,
    output_json: Option<String>,
    claimed_by: Option<String>,
    claimed_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Deserialize)]
struct CreateJobRequest {
    workspace_name: String,
    kind: String,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct CreateWorkspaceRequest {
    name: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default = "default_workspace_cpus")]
    cpus: u8,
    #[serde(default = "default_workspace_memory")]
    memory: u64,
    #[serde(default = "default_workspace_volume_quota")]
    volume_quota: u32,
    #[serde(default = "default_workspace_idle_timeout")]
    idle_timeout: u64,
    #[serde(default = "default_workspace_backup_interval")]
    backup_interval: u64,
    #[serde(default)]
    node_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterNodeRequest {
    node_id: String,
    capacity: NodeCapacity,
}

#[derive(Debug, Deserialize)]
struct ClaimJobRequest {
    node_id: String,
    capacity: NodeCapacity,
    pressure: NodePressure,
}

#[derive(Debug, Deserialize)]
struct CompleteJobRequest {
    node_id: String,
    status: String,
    #[serde(default)]
    output: Value,
}

#[derive(Debug, Deserialize)]
struct JobEventRequest {
    node_id: String,
    event_type: String,
    status: String,
    message: String,
    #[serde(default)]
    metadata: Value,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    node: String,
    db: String,
}

#[derive(Debug, Serialize)]
struct JobResponse {
    job: JobRecord,
}

#[derive(Debug, Deserialize)]
struct WorkerEventsQuery {
    node_id: String,
}

#[derive(Debug, Clone)]
struct ApiState {
    notifier: broadcast::Sender<String>,
}

#[derive(Debug)]
struct BackupArtifact {
    kind: String,
    location: String,
    size_bytes: Option<i64>,
}

trait WorkerTokenExt {
    fn with_worker_token(self) -> Self;
}

impl WorkerTokenExt for reqwest::RequestBuilder {
    fn with_worker_token(self) -> Self {
        match worker_token() {
            Ok(token) if !token.trim().is_empty() => self.bearer_auth(token),
            _ => self,
        }
    }
}

#[derive(Debug, Serialize)]
struct LogRecord<'a> {
    ts: i64,
    level: &'a str,
    node: String,
    event: &'a str,
    workspace: Option<&'a str>,
    message: &'a str,
}

#[derive(Debug, Deserialize)]
struct MomConfig {
    #[serde(default)]
    codex_auth_path: PathBuf,
    #[serde(default = "default_opencode_auth_path")]
    opencode_auth_path: PathBuf,
    #[serde(default = "default_hermes_profile")]
    hermes_profile: String,
    #[serde(default = "default_hermes_model")]
    hermes_model: String,
    #[serde(default = "default_snapshot_name")]
    snapshot_name: String,
    #[serde(default = "default_credential_mode")]
    credential_mode: String,
    #[serde(default)]
    credential_proxy_url: Option<String>,
    #[serde(default)]
    credential_proxy_ca_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialMode {
    VmAuthJson,
    OpenRouterProxy,
}

impl CredentialMode {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "vm-auth-json" | "file" => Ok(Self::VmAuthJson),
            "openrouter-proxy" | "proxy" => Ok(Self::OpenRouterProxy),
            _ => {
                bail!("credential_mode must be one of: vm-auth-json, openrouter-proxy; got {raw:?}")
            }
        }
    }

    fn uses_guest_auth_files(self) -> bool {
        matches!(self, Self::VmAuthJson)
    }

    fn uses_proxy(self) -> bool {
        matches!(self, Self::OpenRouterProxy)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Create(args) => create(args).await,
        Command::List { all } => list(all).await,
        Command::Start { name } => start(&name).await,
        Command::Stop { name } => stop(&name).await,
        Command::Rm { name, all, force } => remove(name.as_deref(), all, force).await,
        Command::Exec { name, command } => {
            let sandbox = running_sandbox(&name).await?;
            run_guest_command(&sandbox, command).await
        }
        Command::Enter { name } => {
            let sandbox = running_sandbox(&name).await?;
            let code = sandbox
                .attach_with("/bin/sh", |a| a.args(["-l"]).env("TERM", "xterm-256color"))
                .await?;
            std::process::exit(code);
        }
        Command::Codex { name, prompt } => {
            let sandbox = running_sandbox(&name).await?;
            run_codex(&sandbox, &prompt.join(" ")).await
        }
        Command::Hermes { name, args } => {
            let sandbox = running_sandbox(&name).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            run_guest_command(&sandbox, command).await
        }
        Command::Doctor { name } => {
            let sandbox = running_sandbox(&name).await?;
            doctor(&sandbox).await
        }
        Command::Workspace { command } => workspace_command(command).await,
        Command::Node { command } => node_command(command).await,
        Command::Api(args) => api(args).await,
        Command::Worker(args) => worker(args).await,
        Command::Daemon(args) => daemon(args).await,
    }
}

async fn create(args: CreateArgs) -> Result<()> {
    create_sandbox(args, None).await
}

async fn create_sandbox(args: CreateArgs, workspace: Option<WorkspaceMount>) -> Result<()> {
    println!("creating {} from {IMAGE}", args.name);
    let config = load_mom_config()?;

    let memory = u32::try_from(args.memory).context("memory must fit in u32 MiB")?;
    if !args.no_snapshot {
        ensure_base_snapshot(&config, args.rebuild_snapshot).await?;
    }

    let mut builder = Sandbox::builder(&args.name)
        .cpus(args.cpus)
        .memory(memory)
        .entrypoint(["tail", "-f", "/dev/null"])
        .shell("/bin/sh")
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"));

    if let Some(mount) = workspace {
        let workspace_name = mount.workspace_name.clone();
        let volume_quota_mib = mount.volume_quota_mib;
        builder = builder
            .label("mom.workspace", &workspace_name)
            .volume("/workspace", |m| {
                m.named_with(mount.volume_name, move |v| {
                    v.ensure_exists()
                        .quota(volume_quota_mib)
                        .label("mom.workspace", workspace_name)
                })
            });
    }

    if args.no_snapshot {
        builder = builder.image(IMAGE);
    } else {
        builder = builder.from_snapshot(&config.snapshot_name);
    }

    if args.replace {
        builder = builder.replace();
    }

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create sandbox '{}'", args.name))?;

    if args.no_snapshot {
        provision_base(&sandbox, &config.hermes_profile).await?;
    } else {
        configure_guest_profile(&sandbox, &config.hermes_profile).await?;
    }
    apply_guest_auth_config(&sandbox, &config).await?;
    if args.no_snapshot {
        doctor(&sandbox).await?;
    }

    println!("stopping {} to persist filesystem changes", args.name);
    sandbox.stop().await?;
    println!("created {}", args.name);
    Ok(())
}

async fn apply_guest_auth_config(sandbox: &Sandbox, config: &MomConfig) -> Result<()> {
    println!("writing VM auth/config from host config");
    let credential_mode = CredentialMode::parse(&config.credential_mode)?;
    validate_credential_config(config, credential_mode)?;

    let (codex_auth, hermes_auth, opencode_auth) = if credential_mode.uses_guest_auth_files() {
        let codex_auth_path = resolve_required_file(&config.codex_auth_path, "codex_auth_path")?;
        let codex_auth = fs::read(&codex_auth_path)
            .with_context(|| format!("read {}", codex_auth_path.display()))?;
        let hermes_auth = codex_auth_as_hermes_auth(&codex_auth_path)?;
        let opencode_auth_path =
            resolve_required_file(&config.opencode_auth_path, "opencode_auth_path")?;
        let opencode_auth = opencode_auth_from_file(&opencode_auth_path)?;
        (Some(codex_auth), Some(hermes_auth), Some(opencode_auth))
    } else {
        (None, None, None)
    };
    let hermes_home = format!("{GUEST_HERMES_HOME}/{}", config.hermes_profile);

    let fs = sandbox.fs();
    fs.mkdir("/workspace").await?;
    fs.mkdir(GUEST_CODEX_HOME).await?;
    fs.mkdir(GUEST_HERMES_HOME).await?;
    fs.mkdir("/root/.local").await?;
    fs.mkdir("/root/.local/share").await?;
    fs.mkdir(GUEST_OPENCODE_DATA_HOME).await?;
    fs.mkdir("/root/.config").await?;
    fs.mkdir(GUEST_OPENCODE_CONFIG_HOME).await?;
    fs.mkdir(&hermes_home).await?;
    fs.mkdir(&format!("{hermes_home}/home")).await?;
    fs.write(
        &format!("{GUEST_CODEX_HOME}/config.toml"),
        codex_config_toml(config).as_bytes(),
    )
    .await?;
    fs.write(
        &format!("{hermes_home}/config.yaml"),
        hermes_config_yaml(config).as_bytes(),
    )
    .await?;
    fs.write(
        &format!("{hermes_home}/SOUL.md"),
        hermes_soul_md().as_bytes(),
    )
    .await?;
    if let Some(codex_auth) = codex_auth {
        fs.write(&format!("{GUEST_CODEX_HOME}/auth.json"), codex_auth)
            .await?;
    }
    if let Some(hermes_auth) = hermes_auth {
        fs.write(&format!("{hermes_home}/auth.json"), hermes_auth.as_bytes())
            .await?;
    }
    if let Some(opencode_auth) = opencode_auth {
        fs.write(
            &format!("{GUEST_OPENCODE_DATA_HOME}/auth.json"),
            opencode_auth.as_bytes(),
        )
        .await?;
    }
    fs.write(
        &format!("{GUEST_OPENCODE_CONFIG_HOME}/opencode.json"),
        opencode_config_json(config).as_bytes(),
    )
    .await?;
    if let Some(proxy_url) = &config.credential_proxy_url {
        fs.write(
            "/etc/profile.d/agentmom-proxy.sh",
            proxy_env_sh(proxy_url).as_bytes(),
        )
        .await?;
    }
    if let Some(ca_path) = &config.credential_proxy_ca_path {
        let ca_path = resolve_required_file(ca_path, "credential_proxy_ca_path")?;
        let ca = fs::read(&ca_path).with_context(|| format!("read {}", ca_path.display()))?;
        fs.mkdir("/usr/local/share/ca-certificates").await?;
        fs.write("/usr/local/share/ca-certificates/agentmom-proxy.crt", ca)
            .await?;
    }

    let hermes_home_q = shell_quote(&hermes_home);
    let auth_chmod = if credential_mode.uses_guest_auth_files() {
        format!(
            "/root/.codex/auth.json {hermes_home_q}/auth.json /root/.local/share/opencode/auth.json"
        )
    } else {
        String::new()
    };
    let remove_guest_auth = if credential_mode.uses_proxy() {
        "rm -f /root/.codex/auth.json /root/.hermes/auth.json /root/.hermes-agent/*/auth.json /root/.local/share/opencode/auth.json"
    } else {
        ":"
    };
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
{remove_guest_auth}
chmod 700 /root/.codex /root/.hermes-agent /root/.local /root/.local/share /root/.local/share/opencode /root/.config /root/.config/opencode {hermes_home_q}
chmod 600 /root/.codex/config.toml {hermes_home_q}/config.yaml {hermes_home_q}/SOUL.md /root/.config/opencode/opencode.json {auth_chmod}
if [ -f /usr/local/share/ca-certificates/agentmom-proxy.crt ]; then update-ca-certificates || true; fi
ln -sfn {hermes_home_q} /root/.hermes
sync
"#
        ),
    )
    .await
}

async fn ensure_base_snapshot(config: &MomConfig, rebuild: bool) -> Result<()> {
    if rebuild {
        println!("rebuilding base snapshot {}", config.snapshot_name);
        let _ = Snapshot::remove(&config.snapshot_name, true).await;
    } else {
        match Snapshot::open(&config.snapshot_name).await {
            Ok(snapshot) => {
                println!(
                    "using base snapshot {} ({})",
                    config.snapshot_name,
                    snapshot.digest()
                );
                return Ok(());
            }
            Err(MicrosandboxError::SnapshotNotFound(_)) => {
                println!(
                    "base snapshot {} not found; building it",
                    config.snapshot_name
                );
            }
            Err(error) => return Err(error).context("open base snapshot"),
        }
    }

    build_base_snapshot(config).await
}

async fn build_base_snapshot(config: &MomConfig) -> Result<()> {
    let hermes_profile_name = config.hermes_profile.clone();

    if let Ok(handle) = Sandbox::get(BASE_BUILDER_NAME).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
        Sandbox::remove(BASE_BUILDER_NAME).await?;
    }

    let builder = Sandbox::builder(BASE_BUILDER_NAME)
        .image(IMAGE)
        .replace()
        .entrypoint(["tail", "-f", "/dev/null"])
        .shell("/bin/sh")
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"))
        .patch(move |patch| {
            let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile_name}");
            patch
                .mkdir("/workspace", Some(0o755))
                .mkdir(GUEST_CODEX_HOME, Some(0o700))
                .mkdir(GUEST_HERMES_HOME, Some(0o700))
                .mkdir("/root/.local", Some(0o700))
                .mkdir("/root/.local/share", Some(0o700))
                .mkdir(GUEST_OPENCODE_DATA_HOME, Some(0o700))
                .mkdir("/root/.config", Some(0o700))
                .mkdir(GUEST_OPENCODE_CONFIG_HOME, Some(0o700))
                .mkdir(&hermes_home, Some(0o700))
                .mkdir(format!("{hermes_home}/home"), Some(0o700))
        });

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create base builder '{BASE_BUILDER_NAME}'"))?;
    provision_base(&sandbox, &config.hermes_profile).await?;
    doctor(&sandbox).await?;
    checked_shell(&sandbox, "sync").await?;

    println!("stopping {BASE_BUILDER_NAME} before snapshot");
    sandbox.stop().await?;

    let snapshot = Snapshot::builder(BASE_BUILDER_NAME)
        .destination(SnapshotDestination::Name(config.snapshot_name.clone()))
        .force()
        .create()
        .await
        .with_context(|| format!("create snapshot '{}'", config.snapshot_name))?;
    println!(
        "created base snapshot {} ({})",
        config.snapshot_name,
        snapshot.digest()
    );

    Sandbox::remove(BASE_BUILDER_NAME).await?;
    Ok(())
}

async fn provision_base(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    println!("installing Alpine packages, uv, Codex, Hermes, and OpenCode");
    checked_shell(
        sandbox,
        r#"
set -eu
apk add --no-cache \
  bash \
  build-base \
  ca-certificates \
  clang \
  compiler-rt \
  curl \
  git \
  libffi-dev \
  nodejs \
  npm \
  python3 \
  python3-dev
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="/root/.local/bin:$PATH"
npm install -g @openai/codex
npm install -g opencode-ai
CC=clang UV_LINK_MODE=copy uv tool install --python 3.13 --force 'hermes-agent[all,messaging]'
ln -sf /root/.local/bin/uv /usr/local/bin/uv
ln -sf /root/.local/bin/uvx /usr/local/bin/uvx
ln -sf /root/.local/bin/hermes /usr/local/bin/hermes
ln -sf /root/.local/bin/hermes-agent /usr/local/bin/hermes-agent
ln -sf /root/.local/bin/hermes-acp /usr/local/bin/hermes-acp
mkdir -p /workspace /root/.codex /root/.hermes-agent /root/.local/share/opencode /root/.config/opencode
"#,
    )
    .await?;
    configure_guest_profile(sandbox, hermes_profile).await
}

async fn configure_guest_profile(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile}");
    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
mkdir -p /workspace /root/.codex /root/.hermes-agent /root/.local/share/opencode /root/.config/opencode {hermes_home_q}
ln -sfn {hermes_home_q} /root/.hermes
cat >/etc/profile.d/mom.sh <<'EOF'
export HERMES_HOME={hermes_home}
export CODEX_HOME=/root/.codex
EOF
cat >/root/.profile <<'EOF'
export HERMES_HOME={hermes_home}
export CODEX_HOME=/root/.codex
EOF
"#
        ),
    )
    .await
}

async fn list(all: bool) -> Result<()> {
    let handles = Sandbox::list().await?;
    println!("{:<24} {:<10} IMAGE", "NAME", "STATUS");
    for handle in handles {
        let config = handle.config()?;
        let managed = config
            .labels
            .get(LABEL_MANAGED)
            .is_some_and(|value| value == "true");
        if !all && !managed {
            continue;
        }

        println!(
            "{:<24} {:<10} {}",
            handle.name(),
            format!("{:?}", handle.status()),
            image_label(&config)
        );
    }
    Ok(())
}

async fn start(name: &str) -> Result<()> {
    let handle = Sandbox::get(name).await?;
    if handle.status() == SandboxStatus::Running {
        println!("{name} already running");
        return Ok(());
    }
    let sandbox = handle.start_detached().await?;
    println!("started {}", sandbox.name());
    Ok(())
}

async fn stop(name: &str) -> Result<()> {
    let handle = Sandbox::get(name).await?;
    handle.stop_with_timeout(Duration::from_secs(10)).await?;
    println!("stopped {name}");
    Ok(())
}

async fn remove(name: Option<&str>, all: bool, force: bool) -> Result<()> {
    if !force {
        bail!("refusing to remove without --force");
    }

    if all {
        if let Some(name) = name {
            bail!("refusing ambiguous remove: pass either {name} or --all, not both");
        }
        return remove_all_managed().await;
    }

    let name = name.ok_or_else(|| anyhow!("missing VM name; pass a name or --all"))?;
    remove_one(name).await
}

async fn remove_one(name: &str) -> Result<()> {
    if let Ok(handle) = Sandbox::get(name).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
    }

    Sandbox::remove(name).await?;
    println!("removed {name}");
    Ok(())
}

async fn remove_all_managed() -> Result<()> {
    let handles = Sandbox::list().await?;
    let mut removed = 0usize;

    for handle in handles {
        let config = handle.config()?;
        let managed = config
            .labels
            .get(LABEL_MANAGED)
            .is_some_and(|value| value == "true");
        if !managed {
            continue;
        }

        let name = handle.name().to_string();
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
        Sandbox::remove(&name).await?;
        println!("removed {name}");
        removed += 1;
    }

    println!("removed {removed} Agent Mom-managed VM(s)");
    Ok(())
}

async fn running_sandbox(name: &str) -> Result<Sandbox> {
    let handle = Sandbox::get(name)
        .await
        .with_context(|| format!("find sandbox '{name}'"))?;
    match handle.status() {
        SandboxStatus::Running | SandboxStatus::Draining => handle
            .connect_with_timeout(Duration::from_secs(30))
            .await
            .with_context(|| format!("connect to running sandbox '{name}'")),
        SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Paused => handle
            .start()
            .await
            .with_context(|| format!("start sandbox '{name}'")),
    }
}

async fn run_codex(sandbox: &Sandbox, prompt: &str) -> Result<()> {
    let config = load_mom_config()?;
    let credential_mode = CredentialMode::parse(&config.credential_mode)?;
    if credential_mode == CredentialMode::OpenRouterProxy {
        bail!(
            "mom codex requires credential_mode vm-auth-json; use Hermes/OpenRouter in openrouter-proxy mode"
        );
    }

    let prompt = shell_quote(prompt);
    let script = format!(
        r#"
set -eu
tmp="$(mktemp -d /root/mom-codex.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT
if [ -f /root/.codex/auth.json ]; then
  cp /root/.codex/auth.json "$tmp/auth.json"
fi
if [ -f /root/.codex/config.toml ]; then
  cp /root/.codex/config.toml "$tmp/config.toml"
fi
if [ -f /etc/profile.d/agentmom-proxy.sh ]; then
  . /etc/profile.d/agentmom-proxy.sh
fi
out="$tmp/last-message.txt"
CODEX_HOME="$tmp" timeout 180 codex exec \
  --ignore-user-config \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  -o "$out" \
  -C /workspace \
  {prompt} </dev/null
cat "$out"
"#
    );
    checked_shell(sandbox, &script).await
}

async fn proxy_smoke(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -eu
if [ -f /root/.codex/auth.json ] || [ -f /root/.hermes/auth.json ]; then
  echo "raw auth files are present in the sandbox" >&2
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

async fn doctor(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -u
echo "== tools =="
node --version
npm --version
uv --version
codex --version
opencode --version
hermes --help >/tmp/mom-hermes-help.txt 2>&1 || true
head -20 /tmp/mom-hermes-help.txt
echo "== codex doctor =="
codex doctor --summary --ascii --no-color || true
"#,
    )
    .await
}

async fn run_guest_command(sandbox: &Sandbox, command: Vec<String>) -> Result<()> {
    let output = capture_guest_command(sandbox, command).await?;
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

async fn capture_guest_command(sandbox: &Sandbox, command: Vec<String>) -> Result<Value> {
    let (cmd, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("missing command"))?;
    let output = sandbox.exec(cmd, args.iter().cloned()).await?;
    let stdout = output.stdout()?;
    let stderr = output.stderr()?;
    let ok = output.status().success;
    let code = output.status().code;
    Ok(json!({
        "ok": ok,
        "code": code,
        "stdout": stdout,
        "stderr": stderr
    }))
}

async fn workspace_command(command: WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::Create(args) => workspace_create(args).await,
        WorkspaceCommand::List => workspace_list(),
        WorkspaceCommand::Inspect { name } => workspace_inspect(&name).await,
        WorkspaceCommand::Events { name, since } => workspace_events_cmd(&name, &since),
        WorkspaceCommand::Start { name } => workspace_start(&name).await,
        WorkspaceCommand::Stop { name } => workspace_stop(&name).await,
        WorkspaceCommand::Exec { name, command } => {
            let workspace = workspace_get(&name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            run_guest_command(&sandbox, command).await
        }
        WorkspaceCommand::Codex { name, prompt } => {
            let workspace = workspace_get(&name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            run_codex(&sandbox, &prompt.join(" ")).await
        }
        WorkspaceCommand::Hermes { name, args } => {
            let workspace = workspace_get(&name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            run_guest_command(&sandbox, command).await
        }
        WorkspaceCommand::RefreshConfig { name } => {
            let workspace = workspace_get(&name)?;
            let config = load_mom_config()?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            apply_guest_auth_config(&sandbox, &config).await?;
            println!("refreshed workspace {name} config");
            Ok(())
        }
        WorkspaceCommand::ProxySmoke { name } => {
            let workspace = workspace_get(&name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            proxy_smoke(&sandbox).await
        }
        WorkspaceCommand::Backup {
            name,
            leave_stopped,
        } => {
            let workspace = workspace_get(&name)?;
            backup_workspace(&workspace, leave_stopped).await
        }
        WorkspaceCommand::Backups { name } => workspace_backups(&name),
        WorkspaceCommand::Restore { name, backup_id } => {
            workspace_restore(&name, backup_id.as_deref()).await
        }
        WorkspaceCommand::Rm {
            name,
            volume,
            force,
        } => workspace_remove(&name, volume, force).await,
    }
}

async fn node_command(command: NodeCommand) -> Result<()> {
    match command {
        NodeCommand::Status => node_status().await,
    }
}

async fn api(args: ApiArgs) -> Result<()> {
    ensure_fleet_schema()?;
    let (notifier, _) = broadcast::channel(1024);
    let state = ApiState { notifier };
    let app = Router::new()
        .route("/health/live", get(api_health_live))
        .route("/health/ready", get(api_health_ready))
        .route("/metrics", get(api_metrics))
        .route("/api/jobs", post(api_create_job))
        .route("/api/jobs/{id}", get(api_get_job))
        .route(
            "/api/workspaces",
            get(api_list_workspaces).post(api_create_workspace),
        )
        .route("/api/workspaces/{name}/events", get(api_workspace_events))
        .route("/worker/register", post(api_worker_register))
        .route("/worker/heartbeat", post(api_worker_register))
        .route("/worker/claim", post(api_worker_claim))
        .route("/worker/jobs/{id}/events", post(api_worker_job_event))
        .route("/worker/jobs/{id}/complete", post(api_worker_job_complete))
        .route("/worker/events", get(api_worker_events))
        .with_state(Arc::new(state));
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("parse API bind address {}", args.bind))?;
    log_record("info", "api_start", None, "Agent Mom API starting");
    println!("Agent Mom API listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn api_health_live() -> Result<Json<HealthResponse>, ApiError> {
    Ok(Json(HealthResponse {
        ok: true,
        node: node_id()?,
        db: fleet_state_dir()?.join("fleet.db").display().to_string(),
    }))
}

async fn api_health_ready() -> Result<Json<HealthResponse>, ApiError> {
    ensure_fleet_schema()?;
    Ok(Json(HealthResponse {
        ok: true,
        node: node_id()?,
        db: fleet_state_dir()?.join("fleet.db").display().to_string(),
    }))
}

async fn api_metrics() -> Result<String, ApiError> {
    let workspaces = workspace_all()?.len();
    let jobs = job_counts()?;
    let backups = backup_count()?;
    Ok(format!(
        "# HELP agentmom_workspaces Total workspaces in the Agent Mom database\n\
         # TYPE agentmom_workspaces gauge\n\
         agentmom_workspaces {workspaces}\n\
         # HELP agentmom_backups_total Backup artifact records\n\
         # TYPE agentmom_backups_total gauge\n\
         agentmom_backups_total {backups}\n\
         # HELP agentmom_jobs Jobs by status\n\
         # TYPE agentmom_jobs gauge\n{}",
        jobs.into_iter()
            .map(|(status, count)| format!(
                "agentmom_jobs{{status=\"{}\"}} {}",
                escape_metric_label(&status),
                count
            ))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

async fn api_create_job(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let job = create_job(request)?;
    let _ = state.notifier.send("job_available".to_string());
    Ok(Json(JobResponse { job }))
}

async fn api_get_job(AxumPath(id): AxumPath<String>) -> Result<Json<JobResponse>, ApiError> {
    Ok(Json(JobResponse { job: job_get(&id)? }))
}

async fn api_list_workspaces() -> Result<Json<Vec<WorkspaceRecord>>, ApiError> {
    Ok(Json(workspace_all()?))
}

async fn api_create_workspace(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let name = sanitize_workspace_name(&request.name)?;
    let job = create_job(CreateJobRequest {
        workspace_name: name,
        kind: "create".to_string(),
        node_id: request.node_id,
        payload: json!({
            "user": request.user,
            "cpus": request.cpus,
            "memory": request.memory,
            "volume_quota": request.volume_quota,
            "idle_timeout": request.idle_timeout,
            "backup_interval": request.backup_interval
        }),
    })?;
    let _ = state.notifier.send("job_available".to_string());
    Ok(Json(JobResponse { job }))
}

async fn api_workspace_events(
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Vec<WorkspaceEvent>>, ApiError> {
    Ok(Json(workspace_events_since(&name, 0)?))
}

async fn api_worker_register(
    headers: HeaderMap,
    Json(request): Json<RegisterNodeRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    register_node(&request.node_id, &request.capacity)?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_claim(
    headers: HeaderMap,
    Json(request): Json<ClaimJobRequest>,
) -> Result<Json<Option<JobRecord>>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    register_node(&request.node_id, &request.capacity)?;
    if !request.pressure.capacity_ok {
        return Ok(Json(None));
    }
    Ok(Json(claim_job(&request.node_id)?))
}

async fn api_worker_job_event(
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<JobEventRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let job = job_get(&id)?;
    record_workspace_event(
        &job.workspace_name,
        &request.event_type,
        &request.status,
        &request.message,
        json!({
            "job_id": id,
            "worker_node_id": request.node_id,
            "metadata": request.metadata
        }),
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_job_complete(
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<CompleteJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let job = complete_job(&id, &request.node_id, &request.status, request.output)?;
    Ok(Json(JobResponse { job }))
}

async fn api_worker_events(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<WorkerEventsQuery>,
) -> Sse<impl tokio_stream::Stream<Item = std::result::Result<SseEvent, Infallible>>> {
    let node_id = query.node_id;
    let stream = BroadcastStream::new(state.notifier.subscribe()).filter_map(move |message| {
        let node_id = node_id.clone();
        match message {
            Ok(kind) => Some(Ok(SseEvent::default()
                .event(kind)
                .data(json!({ "node_id": node_id }).to_string()))),
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn worker(args: WorkerArgs) -> Result<()> {
    ensure_fleet_schema()?;
    let node = node_id()?;
    let client = reqwest::Client::new();
    let api_url = args.api_url.trim_end_matches('/').to_string();
    register_worker(&client, &api_url, &node).await?;
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(32);
    let sse_client = client.clone();
    let sse_url = api_url.clone();
    let sse_node = node.clone();
    tokio::spawn(async move {
        worker_sse_loop(sse_client, sse_url, sse_node, wake_tx).await;
    });

    log_record("info", "worker_start", None, "Agent Mom worker starting");
    loop {
        if worker_claim_once(&client, &api_url, &node).await? && args.once {
            return Ok(());
        }
        if args.once {
            return Ok(());
        }
        tokio::select! {
            _ = wake_rx.recv() => {},
            _ = tokio::time::sleep(Duration::from_secs(args.interval)) => {},
        }
    }
}

async fn register_worker(client: &reqwest::Client, api_url: &str, node: &str) -> Result<()> {
    client
        .post(format!("{api_url}/worker/register"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
            "capacity": node_capacity()
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn worker_sse_loop(
    client: reqwest::Client,
    api_url: String,
    node: String,
    wake_tx: mpsc::Sender<()>,
) {
    loop {
        let url = format!("{api_url}/worker/events?node_id={}", url_component(&node));
        let result = async {
            let response = client.get(url).send().await?.error_for_status()?;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                chunk?;
                let _ = wake_tx.send(()).await;
            }
            Ok::<(), reqwest::Error>(())
        }
        .await;
        if let Err(error) = result {
            eprintln!("worker SSE disconnected: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn worker_claim_once(client: &reqwest::Client, api_url: &str, node: &str) -> Result<bool> {
    let records = workspace_all()?;
    let pressure = node_pressure(&records).await?;
    let response = client
        .post(format!("{api_url}/worker/claim"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
            "capacity": node_capacity(),
            "pressure": pressure
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<JobRecord>>()
        .await?;
    let Some(job) = response else {
        return Ok(false);
    };
    run_claimed_job(client, api_url, node, job).await?;
    Ok(true)
}

async fn run_claimed_job(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    job: JobRecord,
) -> Result<()> {
    worker_job_event(
        client,
        api_url,
        node,
        &job.id,
        "job_running",
        "running",
        "worker started job",
        json!({ "kind": job.kind }),
    )
    .await?;
    let result = execute_job(&job).await;
    match result {
        Ok(output) => {
            client
                .post(format!("{api_url}/worker/jobs/{}/complete", job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": node,
                    "status": "succeeded",
                    "output": output
                }))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
        Err(error) => {
            let message = format!("{error:#}");
            let _ = client
                .post(format!("{api_url}/worker/jobs/{}/complete", job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": node,
                    "status": "failed",
                    "output": { "error": message }
                }))
                .send()
                .await;
            Err(error)
        }
    }
}

async fn worker_job_event(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    job_id: &str,
    event_type: &str,
    status: &str,
    message: &str,
    metadata: Value,
) -> Result<()> {
    client
        .post(format!("{api_url}/worker/jobs/{job_id}/events"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
            "event_type": event_type,
            "status": status,
            "message": message,
            "metadata": metadata
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn execute_job(job: &JobRecord) -> Result<Value> {
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    match job.kind.as_str() {
        "create" => {
            let args = WorkspaceCreateArgs {
                name: job.workspace_name.clone(),
                user: payload
                    .get("user")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                replace: false,
                cpus: payload
                    .get("cpus")
                    .and_then(Value::as_u64)
                    .and_then(|value| u8::try_from(value).ok())
                    .unwrap_or_else(default_workspace_cpus),
                memory: payload
                    .get("memory")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_memory),
                volume_quota: payload
                    .get("volume_quota")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or_else(default_workspace_volume_quota),
                idle_timeout: payload
                    .get("idle_timeout")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_idle_timeout),
                backup_interval: payload
                    .get("backup_interval")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_backup_interval),
                rebuild_snapshot: false,
                no_snapshot: false,
            };
            workspace_create(args).await?;
            Ok(json!({ "created": true }))
        }
        "start" | "warm" => {
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            workspace_set_desired(&workspace.name, "running")?;
            workspace_ensure_running(&workspace).await?;
            Ok(json!({ "started": true }))
        }
        "stop" => {
            workspace_stop(&job.workspace_name).await?;
            Ok(json!({ "stopped": true }))
        }
        "backup" => {
            let workspace = workspace_get(&job.workspace_name)?;
            backup_workspace(&workspace, false).await?;
            Ok(json!({ "backed_up": true }))
        }
        "restore" => {
            let backup_id = payload.get("backup_id").and_then(Value::as_str);
            workspace_restore(&job.workspace_name, backup_id).await?;
            Ok(json!({ "restored": true }))
        }
        "execute" => {
            let command = payload
                .get("command")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("execute job payload requires command array"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .ok_or_else(|| anyhow!("command entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        "codex" => {
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("codex job payload requires prompt"))?;
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            run_codex(&sandbox, prompt).await?;
            Ok(json!({ "ok": true }))
        }
        "hermes" => {
            let empty_args = Vec::new();
            let args = payload
                .get("args")
                .and_then(Value::as_array)
                .unwrap_or(&empty_args)
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .ok_or_else(|| anyhow!("args entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        other => bail!("unknown job kind: {other}"),
    }
}

async fn workspace_create(args: WorkspaceCreateArgs) -> Result<()> {
    let name = sanitize_workspace_name(&args.name)?;
    let sandbox_name = format!("mom-{name}");
    let volume_name = format!("mom-{name}-workspace");
    let memory = u32::try_from(args.memory).context("memory must fit in u32 MiB")?;
    let user_id = args.user.unwrap_or_else(|| name.clone());

    let create_args = CreateArgs {
        name: sandbox_name.clone(),
        replace: args.replace,
        cpus: args.cpus,
        memory: args.memory,
        rebuild_snapshot: args.rebuild_snapshot,
        no_snapshot: args.no_snapshot,
    };
    let mount = WorkspaceMount {
        volume_name: volume_name.clone(),
        volume_quota_mib: args.volume_quota,
        workspace_name: name.clone(),
    };

    workspace_upsert_pending(
        &name,
        &user_id,
        &sandbox_name,
        &volume_name,
        args.cpus,
        memory,
        args.volume_quota,
        args.idle_timeout,
        args.backup_interval,
    )?;
    record_workspace_event(
        &name,
        "workspace_create_started",
        "running",
        "workspace create requested",
        json!({
            "sandbox": sandbox_name,
            "volume": volume_name,
            "cpus": args.cpus,
            "memory_mib": memory,
            "volume_quota_mib": args.volume_quota
        }),
    )?;
    if let Err(error) = create_sandbox(create_args, Some(mount)).await {
        workspace_mark_status(&name, "create-failed")?;
        record_workspace_event(
            &name,
            "workspace_create_failed",
            "failed",
            &format!("{error:#}"),
            json!({ "sandbox": sandbox_name, "volume": volume_name }),
        )?;
        return Err(error);
    }
    workspace_mark_status(&name, "stopped")?;
    record_workspace_event(
        &name,
        "workspace_created",
        "succeeded",
        "workspace VM created and stopped with persistent volume",
        json!({ "sandbox": sandbox_name, "volume": volume_name }),
    )?;
    println!("workspace {name} ready with volume {volume_name}");
    Ok(())
}

fn workspace_list() -> Result<()> {
    let records = workspace_all()?;
    println!(
        "{:<24} {:<16} {:<12} {:<8} {:<8} {:<8} VOLUME",
        "WORKSPACE", "USER", "DESIRED", "CPUS", "MEM", "QUOTA"
    );
    for record in records {
        println!(
            "{:<24} {:<16} {:<12} {:<8} {:<8} {:<8} {}",
            record.name,
            record.user_id,
            record.desired_state,
            record.cpus,
            format!("{}M", record.memory_mib),
            format!("{}M", record.volume_quota_mib),
            record.volume_name
        );
    }
    Ok(())
}

async fn workspace_inspect(name: &str) -> Result<()> {
    let record = workspace_get(name)?;
    let sandbox_status = match Sandbox::get(&record.sandbox_name).await {
        Ok(handle) => format!("{:?}", handle.status()),
        Err(_) => "missing".to_string(),
    };
    let volume_path = microsandbox_volume_path(&record.volume_name)?;
    let events = workspace_recent_events(name, 5)?;

    println!("Workspace: {}", record.name);
    println!("User: {}", record.user_id);
    println!("Node: {}", node_id()?);
    println!("Desired: {}", record.desired_state);
    println!("Status: {}", record.status);
    println!("Sandbox: {}", record.sandbox_name);
    println!("Sandbox status: {sandbox_status}");
    println!("Volume: {}", record.volume_name);
    println!("Volume path: {}", volume_path.display());
    println!("CPUs: {}", record.cpus);
    println!("Memory: {} MiB", record.memory_mib);
    println!("Volume quota: {} MiB", record.volume_quota_mib);
    println!("Idle timeout: {}s", record.idle_timeout_secs);
    println!("Backup interval: {}s", record.backup_interval_secs);
    println!("Last used: {}", record.last_used_at);
    println!(
        "Last backup: {}",
        record
            .last_backup_at
            .map(|value| value.to_string())
            .unwrap_or_else(|| "never".to_string())
    );
    println!("Recent events:");
    for event in events {
        println!(
            "  {} {:<24} {:<10} {}",
            event.created_at, event.event_type, event.status, event.message
        );
    }
    Ok(())
}

fn workspace_events_cmd(name: &str, since: &str) -> Result<()> {
    let since_epoch = now_epoch()?.saturating_sub(parse_duration_secs(since)? as i64);
    let events = workspace_events_since(name, since_epoch)?;
    println!(
        "{:<8} {:<12} {:<16} {:<24} {:<10} MESSAGE",
        "ID", "TIME", "NODE", "EVENT", "STATUS"
    );
    for event in events {
        println!(
            "{:<8} {:<12} {:<16} {:<24} {:<10} {}",
            event.id,
            event.created_at,
            event.node_id,
            event.event_type,
            event.status,
            event.message
        );
        if event.metadata_json != "{}" {
            println!(
                "         workspace={} metadata={}",
                event.workspace_name, event.metadata_json
            );
        }
    }
    Ok(())
}

async fn workspace_start(name: &str) -> Result<()> {
    let workspace = workspace_get(name)?;
    workspace_set_desired(name, "running")?;
    workspace_touch(name)?;
    record_workspace_event(
        name,
        "workspace_start_requested",
        "running",
        "workspace desired state set to running",
        json!({ "sandbox": workspace.sandbox_name }),
    )?;
    workspace_ensure_running(&workspace).await
}

async fn workspace_stop(name: &str) -> Result<()> {
    let workspace = workspace_get(name)?;
    workspace_set_desired(name, "stopped")?;
    if let Ok(handle) = Sandbox::get(&workspace.sandbox_name).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
    }
    workspace_mark_status(name, "stopped")?;
    record_workspace_event(
        name,
        "workspace_stopped",
        "succeeded",
        "workspace stopped",
        json!({ "sandbox": workspace.sandbox_name }),
    )?;
    println!("stopped workspace {name}");
    Ok(())
}

async fn workspace_remove(name: &str, remove_volume: bool, force: bool) -> Result<()> {
    if !force {
        bail!("refusing to remove workspace without --force");
    }
    let workspace = workspace_get(name)?;
    let _ = workspace_stop(name).await;
    let _ = Sandbox::remove(&workspace.sandbox_name).await;
    if remove_volume {
        let _ = microsandbox::Volume::remove(&workspace.volume_name).await;
    }
    record_workspace_event(
        name,
        "workspace_removed",
        "succeeded",
        "workspace record and sandbox removed",
        json!({ "sandbox": workspace.sandbox_name, "volume_removed": remove_volume }),
    )?;
    let db = fleet_db()?;
    db.execute("DELETE FROM workspaces WHERE name = ?1", params![name])?;
    println!("removed workspace {name}");
    Ok(())
}

async fn node_status() -> Result<()> {
    ensure_fleet_schema()?;
    let records = workspace_all()?;
    let capacity = node_capacity();
    let pressure = node_pressure(&records).await?;
    println!("Node: {}", node_id()?);
    println!("State dir: {}", fleet_state_dir()?.display());
    println!("MSB home: {}", microsandbox_home()?.display());
    println!("Workspaces: {}", records.len());
    println!(
        "Capacity: {} CPU, {} MiB memory, {} active workspaces, {} MiB disk reserve",
        capacity.cpus,
        capacity.memory_mib,
        capacity.max_active_workspaces,
        capacity.disk_reserve_mib
    );
    println!("Managed sandboxes: {}", pressure.managed_sandboxes);
    println!("Running sandboxes: {}", pressure.running_sandboxes);
    println!("Active workspaces: {}", pressure.active_workspaces);
    println!(
        "Allocated running memory: {} MiB",
        pressure.allocated_memory_mib
    );
    if let Some(available) = pressure.disk_available_mib {
        println!("Disk available: {available} MiB");
    }
    println!("Capacity OK: {}", pressure.capacity_ok);
    println!("Disk:");
    let state_dir = fleet_state_dir()?.display().to_string();
    for line in command_stdout("df", &["-h".to_string(), state_dir]).await? {
        println!("  {line}");
    }
    Ok(())
}

async fn daemon(args: DaemonArgs) -> Result<()> {
    ensure_fleet_schema()?;
    log_record("info", "daemon_start", None, "Agent Mom daemon starting");
    loop {
        daemon_once().await?;
        if args.once {
            log_record(
                "info",
                "daemon_once_complete",
                None,
                "daemon one-shot pass complete",
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(args.interval)).await;
    }
}

fn node_capacity() -> NodeCapacity {
    NodeCapacity {
        cpus: env_u32("MOM_CAPACITY_CPUS", 0),
        memory_mib: env_u64("MOM_CAPACITY_MEMORY_MIB", 0),
        max_active_workspaces: env_u32("MOM_CAPACITY_ACTIVE_WORKSPACES", 0),
        disk_reserve_mib: env_u64("MOM_CAPACITY_DISK_RESERVE_MIB", 10 * 1024),
    }
}

async fn node_pressure(records: &[WorkspaceRecord]) -> Result<NodePressure> {
    let sandboxes = Sandbox::list().await.unwrap_or_default();
    let running_sandboxes = sandboxes
        .iter()
        .filter(|handle| handle.status() == SandboxStatus::Running)
        .count();
    let managed_sandboxes = sandboxes
        .iter()
        .filter(|handle| {
            handle
                .config()
                .ok()
                .and_then(|config| config.labels.get(LABEL_MANAGED).cloned())
                .is_some_and(|value| value == "true")
        })
        .count();
    let running_workspace_names: Vec<_> = sandboxes
        .iter()
        .filter(|handle| handle.status() == SandboxStatus::Running)
        .filter_map(|handle| {
            handle
                .config()
                .ok()
                .and_then(|config| config.labels.get("mom.workspace").cloned())
        })
        .collect();
    let active_workspaces = running_workspace_names.len();
    let allocated_memory_mib = records
        .iter()
        .filter(|record| {
            running_workspace_names
                .iter()
                .any(|name| name == &record.name)
        })
        .map(|record| u64::from(record.memory_mib))
        .sum();
    let disk_available_mib = disk_available_mib().await.ok();
    let capacity = node_capacity();
    let active_ok = capacity.max_active_workspaces == 0
        || active_workspaces < capacity.max_active_workspaces as usize;
    let memory_ok = capacity.memory_mib == 0 || allocated_memory_mib < capacity.memory_mib;
    let disk_ok = disk_available_mib
        .map(|available| available > capacity.disk_reserve_mib)
        .unwrap_or(true);

    Ok(NodePressure {
        active_workspaces,
        running_sandboxes,
        managed_sandboxes,
        allocated_memory_mib,
        disk_available_mib,
        capacity_ok: active_ok && memory_ok && disk_ok,
    })
}

async fn disk_available_mib() -> Result<u64> {
    let state_dir = fleet_state_dir()?.display().to_string();
    let lines = command_stdout("df", &["-Pm".to_string(), state_dir]).await?;
    let data = lines
        .get(1)
        .ok_or_else(|| anyhow!("df did not return a data row"))?;
    let fields: Vec<_> = data.split_whitespace().collect();
    let available = fields
        .get(3)
        .ok_or_else(|| anyhow!("df row missing available column: {data}"))?;
    available.parse().context("parse df available MiB")
}

async fn daemon_once() -> Result<()> {
    let records = workspace_all()?;
    let now = now_epoch()?;
    for record in records {
        if let Err(error) = daemon_reconcile_workspace(&record, now).await {
            log_record(
                "error",
                "workspace_reconcile_failed",
                Some(&record.name),
                "workspace reconciliation failed",
            );
            workspace_mark_status(&record.name, "error")?;
            record_workspace_event(
                &record.name,
                "workspace_reconcile_failed",
                "failed",
                &format!("{error:#}"),
                json!({ "sandbox": record.sandbox_name, "volume": record.volume_name }),
            )?;
            eprintln!("reconcile {} failed: {error:#}", record.name);
        }
    }
    Ok(())
}

async fn daemon_reconcile_workspace(record: &WorkspaceRecord, now: i64) -> Result<()> {
    if record.desired_state == "running" {
        workspace_ensure_running(record).await?;
        if record.idle_timeout_secs > 0
            && now.saturating_sub(record.last_used_at) >= record.idle_timeout_secs as i64
        {
            log_record(
                "info",
                "workspace_idle_stop",
                Some(&record.name),
                "workspace idle timeout reached",
            );
            println!(
                "workspace {} idle for {}s; stopping",
                record.name,
                now.saturating_sub(record.last_used_at)
            );
            if let Ok(handle) = Sandbox::get(&record.sandbox_name).await {
                if handle.status() == SandboxStatus::Running
                    || handle.status() == SandboxStatus::Draining
                {
                    handle.stop_with_timeout(Duration::from_secs(10)).await?;
                }
            }
            workspace_mark_status(&record.name, "idle-stopped")?;
            record_workspace_event(
                &record.name,
                "workspace_idle_stopped",
                "succeeded",
                "workspace stopped after idle timeout",
                json!({ "idle_seconds": now.saturating_sub(record.last_used_at) }),
            )?;
        }
    }

    if backup_due(record, now) {
        if let Err(error) = backup_workspace(record, false).await {
            log_record(
                "error",
                "workspace_backup_failed",
                Some(&record.name),
                "workspace backup failed",
            );
            record_workspace_event(
                &record.name,
                "workspace_backup_failed",
                "failed",
                &format!("{error:#}"),
                json!({}),
            )?;
            eprintln!("backup {} failed: {error:#}", record.name);
        }
    }
    Ok(())
}

async fn workspace_ensure_running(workspace: &WorkspaceRecord) -> Result<()> {
    match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) if handle.status() == SandboxStatus::Running => {
            workspace_mark_status(&workspace.name, "running")?;
            Ok(())
        }
        Ok(handle) => {
            log_record(
                "info",
                "workspace_starting",
                Some(&workspace.name),
                "starting workspace sandbox",
            );
            record_workspace_event(
                &workspace.name,
                "sandbox_starting",
                "running",
                "starting workspace sandbox",
                json!({ "sandbox": workspace.sandbox_name }),
            )?;
            let sandbox = handle.start_detached().await?;
            println!("started workspace {} as {}", workspace.name, sandbox.name());
            workspace_mark_status(&workspace.name, "running")?;
            record_workspace_event(
                &workspace.name,
                "sandbox_started",
                "succeeded",
                "workspace sandbox started",
                json!({ "sandbox": workspace.sandbox_name }),
            )?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "workspace {} has no sandbox {}; recreate it",
                workspace.name, workspace.sandbox_name
            )
        }),
    }
}

async fn workspace_running_sandbox(workspace: &WorkspaceRecord) -> Result<Sandbox> {
    match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) => match handle.status() {
            SandboxStatus::Running | SandboxStatus::Draining => handle
                .connect_with_timeout(Duration::from_secs(30))
                .await
                .with_context(|| {
                    format!("connect to running sandbox '{}'", workspace.sandbox_name)
                }),
            SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Paused => {
                log_record(
                    "info",
                    "workspace_starting",
                    Some(&workspace.name),
                    "starting workspace sandbox",
                );
                record_workspace_event(
                    &workspace.name,
                    "sandbox_starting",
                    "running",
                    "starting workspace sandbox",
                    json!({ "sandbox": workspace.sandbox_name }),
                )?;
                let sandbox = handle
                    .start()
                    .await
                    .with_context(|| format!("start sandbox '{}'", workspace.sandbox_name))?;
                workspace_mark_status(&workspace.name, "running")?;
                record_workspace_event(
                    &workspace.name,
                    "sandbox_started",
                    "succeeded",
                    "workspace sandbox started",
                    json!({ "sandbox": workspace.sandbox_name }),
                )?;
                Ok(sandbox)
            }
        },
        Err(error) => Err(error).with_context(|| {
            format!(
                "workspace {} has no sandbox {}; recreate it",
                workspace.name, workspace.sandbox_name
            )
        }),
    }
}

async fn backup_workspace(workspace: &WorkspaceRecord, leave_stopped: bool) -> Result<()> {
    let was_running = match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) => {
            let running = handle.status() == SandboxStatus::Running
                || handle.status() == SandboxStatus::Draining;
            if running {
                record_workspace_event(
                    &workspace.name,
                    "backup_stop_started",
                    "running",
                    "stopping workspace before backup",
                    json!({ "sandbox": workspace.sandbox_name }),
                )?;
                println!("stopping {} before backup", workspace.sandbox_name);
                handle.stop_with_timeout(Duration::from_secs(20)).await?;
                workspace_mark_status(&workspace.name, "backup-stopped")?;
            }
            running
        }
        Err(_) => false,
    };

    let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
    if !volume_path.exists() {
        bail!(
            "workspace volume {} does not exist at {}",
            workspace.volume_name,
            volume_path.display()
        );
    }
    log_record(
        "info",
        "workspace_backup_started",
        Some(&workspace.name),
        "workspace backup started",
    );
    record_workspace_event(
        &workspace.name,
        "workspace_backup_started",
        "running",
        "workspace volume backup started",
        json!({ "volume": workspace.volume_name }),
    )?;
    let artifact = run_backup_command(workspace, &volume_path).await?;
    let backup_id = record_backup_artifact(workspace, &artifact, "succeeded")?;
    workspace_mark_backup(&workspace.name)?;
    record_workspace_event(
        &workspace.name,
        "workspace_backup_succeeded",
        "succeeded",
        "workspace volume backup completed",
        json!({
            "volume": workspace.volume_name,
            "backup_id": backup_id,
            "kind": artifact.kind,
            "location": artifact.location
        }),
    )?;

    if was_running && !leave_stopped && workspace.desired_state == "running" {
        workspace_ensure_running(workspace).await?;
    }
    Ok(())
}

async fn run_backup_command(
    workspace: &WorkspaceRecord,
    volume_path: &Path,
) -> Result<BackupArtifact> {
    if let Ok(command) = env::var("MOM_BACKUP_COMMAND") {
        println!("running MOM_BACKUP_COMMAND for {}", workspace.name);
        let status = TokioCommand::new("sh")
            .arg("-c")
            .arg(command)
            .env("MOM_WORKSPACE", &workspace.name)
            .env("MOM_VOLUME", &workspace.volume_name)
            .env("MOM_VOLUME_PATH", volume_path)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("MOM_BACKUP_COMMAND exited with {status}");
        }
        return Ok(BackupArtifact {
            kind: "command".to_string(),
            location: env::var("MOM_BACKUP_LOCATION").unwrap_or_else(|_| "external".to_string()),
            size_bytes: None,
        });
    }

    if env::var_os("RESTIC_REPOSITORY").is_some() && command_exists("restic").await {
        println!("running restic backup for {}", workspace.name);
        let status = TokioCommand::new("restic")
            .arg("backup")
            .arg(volume_path)
            .arg("--tag")
            .arg("agentmom")
            .arg("--tag")
            .arg(&workspace.name)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("restic backup exited with {status}");
        }
        return Ok(BackupArtifact {
            kind: "restic".to_string(),
            location: env::var("RESTIC_REPOSITORY").unwrap_or_else(|_| "restic".to_string()),
            size_bytes: None,
        });
    }

    if command_exists("kopia").await {
        println!("running kopia snapshot for {}", workspace.name);
        let status = TokioCommand::new("kopia")
            .arg("snapshot")
            .arg("create")
            .arg(volume_path)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("kopia snapshot create exited with {status}");
        }
        return Ok(BackupArtifact {
            kind: "kopia".to_string(),
            location: volume_path.display().to_string(),
            size_bytes: None,
        });
    }

    local_tar_backup(workspace, volume_path).await
}

async fn local_tar_backup(
    workspace: &WorkspaceRecord,
    volume_path: &Path,
) -> Result<BackupArtifact> {
    let backup_dir = fleet_state_dir()?.join("backups").join(&workspace.name);
    fs::create_dir_all(&backup_dir)?;
    let archive = backup_dir.join(format!("{}-{}.tar", workspace.name, now_epoch()?));
    println!(
        "no restic/kopia configured; writing local backup {}",
        archive.display()
    );
    let parent = volume_path
        .parent()
        .ok_or_else(|| anyhow!("volume path has no parent: {}", volume_path.display()))?;
    let name = volume_path.file_name().ok_or_else(|| {
        anyhow!(
            "volume path has no final component: {}",
            volume_path.display()
        )
    })?;
    let status = TokioCommand::new("tar")
        .arg("-cf")
        .arg(&archive)
        .arg("-C")
        .arg(parent)
        .arg(name)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("tar backup exited with {status}");
    }
    let size_bytes = fs::metadata(&archive)
        .ok()
        .map(|metadata| metadata.len() as i64);
    Ok(BackupArtifact {
        kind: "local-tar".to_string(),
        location: archive.display().to_string(),
        size_bytes,
    })
}

fn workspace_backups(name: &str) -> Result<()> {
    let backups = backup_records_for_workspace(name)?;
    println!(
        "{:<28} {:<12} {:<10} {:<12} LOCATION",
        "ID", "KIND", "STATUS", "CREATED"
    );
    for backup in backups {
        println!(
            "{:<28} {:<12} {:<10} {:<12} {}",
            backup.id, backup.kind, backup.status, backup.created_at, backup.location
        );
    }
    Ok(())
}

async fn workspace_restore(name: &str, backup_id: Option<&str>) -> Result<()> {
    let workspace = workspace_get(name)?;
    workspace_stop(name).await?;
    let backup = match backup_id {
        Some(id) => backup_record_get(id)?,
        None => latest_local_tar_backup(name)?,
    };
    if backup.kind != "local-tar" {
        bail!(
            "restore currently supports local-tar artifacts only; backup {} is {}",
            backup.id,
            backup.kind
        );
    }
    let archive = PathBuf::from(&backup.location);
    if !archive.exists() {
        bail!("backup artifact is missing: {}", archive.display());
    }
    let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
    if volume_path.exists() {
        fs::remove_dir_all(&volume_path)
            .with_context(|| format!("remove existing volume path {}", volume_path.display()))?;
    }
    let parent = volume_path
        .parent()
        .ok_or_else(|| anyhow!("volume path has no parent: {}", volume_path.display()))?;
    fs::create_dir_all(parent)?;
    let status = TokioCommand::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(parent)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("tar restore exited with {status}");
    }
    workspace_mark_status(name, "restored")?;
    record_workspace_event(
        name,
        "workspace_restored",
        "succeeded",
        "workspace volume restored from backup",
        json!({ "backup_id": backup.id, "location": backup.location }),
    )?;
    println!("restored workspace {name} from {}", backup.id);
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

async fn checked_shell(sandbox: &Sandbox, script: &str) -> Result<()> {
    let output = sandbox.shell(script).await?;
    print!("{}", output.stdout()?);
    eprint!("{}", output.stderr()?);
    if !output.status().success {
        bail!("guest shell command exited with {}", output.status().code);
    }
    Ok(())
}

fn codex_auth_as_hermes_auth(auth_path: &PathBuf) -> Result<String> {
    let raw =
        fs::read_to_string(&auth_path).with_context(|| format!("read {}", auth_path.display()))?;
    let codex_auth: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", auth_path.display()))?;
    let tokens = codex_auth
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{} does not contain a tokens object", auth_path.display()))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.access_token", auth_path.display()))?;
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.refresh_token", auth_path.display()))?;
    let last_refresh = codex_auth
        .get("last_refresh")
        .and_then(Value::as_str)
        .unwrap_or("2026-01-01T00:00:00Z");
    let account_id = codex_auth
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or("codex-cli");

    let payload = json!({
        "version": 1,
        "active_provider": "openai-codex",
        "providers": {
            "openai-codex": {
                "tokens": tokens,
                "last_refresh": last_refresh,
                "auth_mode": "chatgpt",
                "label": "Codex CLI"
            }
        },
        "credential_pool": {
            "openai-codex": [
                {
                    "id": format!("codex-cli-{account_id}"),
                    "label": "Codex CLI",
                    "source": "device_code",
                    "auth_type": "oauth",
                    "priority": 0,
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "last_refresh": last_refresh,
                    "last_status": Value::Null,
                    "last_status_at": Value::Null,
                    "last_error_code": Value::Null,
                    "last_error_reason": Value::Null,
                    "last_error_message": Value::Null,
                    "last_error_reset_at": Value::Null
                }
            ]
        }
    });

    println!(
        "will seed Hermes OpenAI Codex auth from {}",
        auth_path.display()
    );
    Ok(serde_json::to_string_pretty(&payload)?)
}

fn ensure_fleet_schema() -> Result<()> {
    let db = fleet_db()?;
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS workspaces (
    name TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    sandbox_name TEXT NOT NULL UNIQUE,
    volume_name TEXT NOT NULL UNIQUE,
    desired_state TEXT NOT NULL,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    volume_quota_mib INTEGER NOT NULL,
    status TEXT NOT NULL,
    idle_timeout_secs INTEGER NOT NULL,
    backup_interval_secs INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    last_backup_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_name TEXT NOT NULL,
    node_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    status TEXT NOT NULL,
    message TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workspace_events_workspace_created
ON workspace_events (workspace_name, created_at);

CREATE TABLE IF NOT EXISTS workspace_backups (
    id TEXT PRIMARY KEY,
    workspace_name TEXT NOT NULL,
    node_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    location TEXT NOT NULL,
    status TEXT NOT NULL,
    size_bytes INTEGER,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workspace_backups_workspace_created
ON workspace_backups (workspace_name, created_at);

CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    max_active_workspaces INTEGER NOT NULL,
    disk_reserve_mib INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    status TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    workspace_name TEXT NOT NULL,
    node_id TEXT,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    output_json TEXT,
    claimed_by TEXT,
    claimed_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_jobs_claim
ON jobs (status, node_id, created_at);
"#,
    )?;
    Ok(())
}

fn workspace_upsert_pending(
    name: &str,
    user_id: &str,
    sandbox_name: &str,
    volume_name: &str,
    cpus: u8,
    memory_mib: u32,
    volume_quota_mib: u32,
    idle_timeout_secs: u64,
    backup_interval_secs: u64,
) -> Result<()> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO workspaces (
    name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
    volume_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, 'creating', ?8, ?9, ?10, NULL, ?10, ?10)
ON CONFLICT(name) DO UPDATE SET
    user_id = excluded.user_id,
    sandbox_name = excluded.sandbox_name,
    volume_name = excluded.volume_name,
    desired_state = excluded.desired_state,
    cpus = excluded.cpus,
    memory_mib = excluded.memory_mib,
    volume_quota_mib = excluded.volume_quota_mib,
    status = excluded.status,
    idle_timeout_secs = excluded.idle_timeout_secs,
    backup_interval_secs = excluded.backup_interval_secs,
    updated_at = excluded.updated_at
"#,
        params![
            name,
            user_id,
            sandbox_name,
            volume_name,
            i64::from(cpus),
            i64::from(memory_mib),
            i64::from(volume_quota_mib),
            i64::try_from(idle_timeout_secs).context("idle timeout too large")?,
            i64::try_from(backup_interval_secs).context("backup interval too large")?,
            now,
        ],
    )?;
    Ok(())
}

fn workspace_get(name: &str) -> Result<WorkspaceRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
       status, volume_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at
FROM workspaces
WHERE name = ?1
"#,
        params![name],
        workspace_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("workspace not found: {name}"))
}

fn workspace_all() -> Result<Vec<WorkspaceRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
       status, volume_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at
FROM workspaces
ORDER BY name
"#,
    )?;
    let records = stmt
        .query_map([], workspace_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(records)
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let cpus: i64 = row.get(5)?;
    let memory_mib: i64 = row.get(6)?;
    let volume_quota_mib: i64 = row.get(8)?;
    let idle_timeout_secs: i64 = row.get(9)?;
    let backup_interval_secs: i64 = row.get(10)?;
    Ok(WorkspaceRecord {
        name: row.get(0)?,
        user_id: row.get(1)?,
        sandbox_name: row.get(2)?,
        volume_name: row.get(3)?,
        desired_state: row.get(4)?,
        status: row.get(7)?,
        cpus: cpus as u8,
        memory_mib: memory_mib as u32,
        volume_quota_mib: volume_quota_mib as u32,
        idle_timeout_secs: idle_timeout_secs as u64,
        backup_interval_secs: backup_interval_secs as u64,
        last_used_at: row.get(11)?,
        last_backup_at: row.get(12)?,
    })
}

fn workspace_set_desired(name: &str, desired_state: &str) -> Result<()> {
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET desired_state = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, desired_state, now_epoch()?],
    )?;
    Ok(())
}

fn workspace_touch(name: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET last_used_at = ?2, updated_at = ?2 WHERE name = ?1",
        params![name, now],
    )?;
    Ok(())
}

fn workspace_mark_status(name: &str, status: &str) -> Result<()> {
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET status = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, status, now_epoch()?],
    )?;
    Ok(())
}

fn workspace_mark_backup(name: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET last_backup_at = ?2, updated_at = ?2 WHERE name = ?1",
        params![name, now],
    )?;
    Ok(())
}

fn record_workspace_event(
    workspace_name: &str,
    event_type: &str,
    status: &str,
    message: &str,
    metadata: Value,
) -> Result<()> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO workspace_events (
    workspace_name, node_id, event_type, status, message, metadata_json, created_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#,
        params![
            workspace_name,
            node_id()?,
            event_type,
            status,
            message,
            serde_json::to_string(&metadata)?,
            now_epoch()?,
        ],
    )?;
    Ok(())
}

fn workspace_events_since(name: &str, since_epoch: i64) -> Result<Vec<WorkspaceEvent>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT id, workspace_name, node_id, event_type, status, message, metadata_json, created_at
FROM workspace_events
WHERE workspace_name = ?1 AND created_at >= ?2
ORDER BY created_at ASC, id ASC
"#,
    )?;
    let events = stmt
        .query_map(params![name, since_epoch], workspace_event_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(events)
}

fn workspace_recent_events(name: &str, limit: u32) -> Result<Vec<WorkspaceEvent>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT id, workspace_name, node_id, event_type, status, message, metadata_json, created_at
FROM workspace_events
WHERE workspace_name = ?1
ORDER BY created_at DESC, id DESC
LIMIT ?2
"#,
    )?;
    let mut events = stmt
        .query_map(params![name, i64::from(limit)], workspace_event_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    events.reverse();
    Ok(events)
}

fn workspace_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceEvent> {
    Ok(WorkspaceEvent {
        id: row.get(0)?,
        workspace_name: row.get(1)?,
        node_id: row.get(2)?,
        event_type: row.get(3)?,
        status: row.get(4)?,
        message: row.get(5)?,
        metadata_json: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn record_backup_artifact(
    workspace: &WorkspaceRecord,
    artifact: &BackupArtifact,
    status: &str,
) -> Result<String> {
    ensure_fleet_schema()?;
    let id = new_id("bak")?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO workspace_backups (
    id, workspace_name, node_id, kind, location, status, size_bytes, created_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
"#,
        params![
            id,
            workspace.name,
            node_id()?,
            artifact.kind,
            artifact.location,
            status,
            artifact.size_bytes,
            now_epoch()?,
        ],
    )?;
    Ok(id)
}

fn backup_records_for_workspace(name: &str) -> Result<Vec<BackupRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT id, workspace_name, node_id, kind, location, status, size_bytes, created_at
FROM workspace_backups
WHERE workspace_name = ?1
ORDER BY created_at DESC
"#,
    )?;
    Ok(stmt
        .query_map(params![name], backup_record_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn backup_record_get(id: &str) -> Result<BackupRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, location, status, size_bytes, created_at
FROM workspace_backups
WHERE id = ?1
"#,
        params![id],
        backup_record_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("backup not found: {id}"))
}

fn latest_local_tar_backup(name: &str) -> Result<BackupRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, location, status, size_bytes, created_at
FROM workspace_backups
WHERE workspace_name = ?1 AND kind = 'local-tar' AND status = 'succeeded'
ORDER BY created_at DESC
LIMIT 1
"#,
        params![name],
        backup_record_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("no local-tar backup found for workspace {name}"))
}

fn backup_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRecord> {
    Ok(BackupRecord {
        id: row.get(0)?,
        workspace_name: row.get(1)?,
        node_id: row.get(2)?,
        kind: row.get(3)?,
        location: row.get(4)?,
        status: row.get(5)?,
        size_bytes: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn create_job(request: CreateJobRequest) -> Result<JobRecord> {
    ensure_fleet_schema()?;
    let workspace_name = sanitize_workspace_name(&request.workspace_name)?;
    let id = new_id("job")?;
    let now = now_epoch()?;
    let payload_json = serde_json::to_string(&request.payload)?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO jobs (
    id, workspace_name, node_id, kind, status, payload_json, output_json,
    claimed_by, claimed_at, created_at, updated_at
) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, NULL, NULL, NULL, ?6, ?6)
"#,
        params![
            id,
            workspace_name,
            request.node_id,
            request.kind,
            payload_json,
            now,
        ],
    )?;
    job_get(&id)
}

fn job_get(id: &str) -> Result<JobRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, status, payload_json, output_json,
       claimed_by, claimed_at, created_at, updated_at
FROM jobs
WHERE id = ?1
"#,
        params![id],
        job_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("job not found: {id}"))
}

fn claim_job(node: &str) -> Result<Option<JobRecord>> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        r#"
UPDATE jobs
SET status = 'claimed', claimed_by = ?1, claimed_at = ?2, updated_at = ?2
WHERE id = (
    SELECT id
    FROM jobs
    WHERE status = 'queued' AND (node_id IS NULL OR node_id = ?1)
    ORDER BY created_at ASC
    LIMIT 1
)
"#,
        params![node, now],
    )?;
    db.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, status, payload_json, output_json,
       claimed_by, claimed_at, created_at, updated_at
FROM jobs
WHERE status = 'claimed' AND claimed_by = ?1 AND claimed_at = ?2
ORDER BY created_at ASC
LIMIT 1
"#,
        params![node, now],
        job_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn complete_job(id: &str, node: &str, status: &str, output: Value) -> Result<JobRecord> {
    if !matches!(status, "succeeded" | "failed" | "canceled") {
        bail!("invalid terminal job status: {status}");
    }
    let now = now_epoch()?;
    let output_json = serde_json::to_string(&output)?;
    let db = fleet_db()?;
    let changed = db.execute(
        r#"
UPDATE jobs
SET status = ?3, output_json = ?4, updated_at = ?5
WHERE id = ?1 AND claimed_by = ?2 AND status IN ('claimed', 'running')
"#,
        params![id, node, status, output_json, now],
    )?;
    if changed == 0 {
        bail!("job {id} is not claimed by node {node}");
    }
    job_get(id)
}

fn register_node(node: &str, capacity: &NodeCapacity) -> Result<()> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO nodes (
    node_id, cpus, memory_mib, max_active_workspaces, disk_reserve_mib, last_seen_at, status
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'ready')
ON CONFLICT(node_id) DO UPDATE SET
    cpus = excluded.cpus,
    memory_mib = excluded.memory_mib,
    max_active_workspaces = excluded.max_active_workspaces,
    disk_reserve_mib = excluded.disk_reserve_mib,
    last_seen_at = excluded.last_seen_at,
    status = excluded.status
"#,
        params![
            node,
            i64::from(capacity.cpus),
            i64::try_from(capacity.memory_mib).context("memory capacity too large")?,
            i64::from(capacity.max_active_workspaces),
            i64::try_from(capacity.disk_reserve_mib).context("disk reserve too large")?,
            now_epoch()?,
        ],
    )?;
    Ok(())
}

fn job_counts() -> Result<Vec<(String, i64)>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare("SELECT status, COUNT(*) FROM jobs GROUP BY status")?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn backup_count() -> Result<i64> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    Ok(
        db.query_row("SELECT COUNT(*) FROM workspace_backups", [], |row| {
            row.get(0)
        })?,
    )
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: row.get(0)?,
        workspace_name: row.get(1)?,
        node_id: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        payload_json: row.get(5)?,
        output_json: row.get(6)?,
        claimed_by: row.get(7)?,
        claimed_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn backup_due(workspace: &WorkspaceRecord, now: i64) -> bool {
    if workspace.backup_interval_secs == 0 {
        return false;
    }
    match workspace.last_backup_at {
        Some(last) => now.saturating_sub(last) >= workspace.backup_interval_secs as i64,
        None => true,
    }
}

fn fleet_db() -> Result<Connection> {
    let dir = fleet_state_dir()?;
    fs::create_dir_all(&dir)?;
    let db = Connection::open(dir.join("fleet.db"))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    Ok(db)
}

fn fleet_state_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".local").join("state").join("mom"))
}

fn microsandbox_volume_path(volume_name: &str) -> Result<PathBuf> {
    Ok(microsandbox_home()?.join("volumes").join(volume_name))
}

fn microsandbox_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MSB_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".microsandbox"))
}

fn node_id() -> Result<String> {
    if let Ok(value) = env::var("MOM_NODE_ID") {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }
    if let Ok(value) = env::var("HOSTNAME") {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }
    let output = std::process::Command::new("hostname").output();
    if let Ok(output) = output {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !hostname.is_empty() {
            return Ok(hostname);
        }
    }
    Ok("unknown".to_string())
}

fn log_record(level: &str, event: &str, workspace: Option<&str>, message: &str) {
    if env::var("MOM_LOG_FORMAT").is_ok_and(|value| value == "json") {
        let record = LogRecord {
            ts: now_epoch().unwrap_or_default(),
            level,
            node: node_id().unwrap_or_else(|_| "unknown".to_string()),
            event,
            workspace,
            message,
        };
        match serde_json::to_string(&record) {
            Ok(line) => eprintln!("{line}"),
            Err(_) => eprintln!("{level} {event} {message}"),
        }
    } else {
        match workspace {
            Some(workspace) => eprintln!("{level} {event} workspace={workspace} {message}"),
            None => eprintln!("{level} {event} {message}"),
        }
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn require_worker_token(headers: &HeaderMap) -> Result<()> {
    if env::var_os("MOM_WORKER_TOKEN").is_none() && env::var_os("MOM_WORKER_TOKEN_FILE").is_none() {
        return Ok(());
    }
    let expected = worker_token()?;
    if expected.trim().is_empty() {
        return Ok(());
    }
    let actual = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| anyhow!("missing worker bearer token"))?;
    if actual != expected {
        bail!("invalid worker bearer token");
    }
    Ok(())
}

fn worker_token() -> Result<String> {
    if let Ok(value) = env::var("MOM_WORKER_TOKEN") {
        return Ok(value);
    }
    if let Some(path) = env::var_os("MOM_WORKER_TOKEN_FILE") {
        return Ok(fs::read_to_string(PathBuf::from(path))?.trim().to_string());
    }
    bail!("worker token is not configured")
}

fn new_id(prefix: &str) -> Result<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{prefix}-{nanos}-{}", std::process::id()))
}

fn escape_metric_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn url_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
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

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
enum ApiError {
    Anyhow(anyhow::Error),
    Unauthorized(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Anyhow(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Anyhow(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: format!("{error:#}"),
                }),
            )
                .into_response(),
            ApiError::Unauthorized(error) => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorBody {
                    error: format!("{error:#}"),
                }),
            )
                .into_response(),
        }
    }
}

fn parse_duration_secs(value: &str) -> Result<u64> {
    let value = value.trim();
    if value.is_empty() {
        bail!("duration must not be empty");
    }
    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60),
        Some(b'd') => (&value[..value.len() - 1], 24 * 60 * 60),
        Some(byte) if byte.is_ascii_digit() => (value, 1),
        _ => bail!("duration must end with s, m, h, d, or be bare seconds"),
    };
    let amount: u64 = number.parse().context("parse duration amount")?;
    Ok(amount.saturating_mul(multiplier))
}

async fn command_stdout(command: &str, args: &[String]) -> Result<Vec<String>> {
    let output = TokioCommand::new(command)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {command}"))?;
    if !output.status.success() {
        bail!("{command} exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToString::to_string)
        .collect())
}

fn sanitize_workspace_name(name: &str) -> Result<String> {
    if name.is_empty() {
        bail!("workspace name must not be empty");
    }
    if name.len() > 96 {
        bail!("workspace name must be at most 96 bytes");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        bail!(
            "workspace name may contain only ASCII letters, numbers, dots, hyphens, and underscores"
        );
    }
    Ok(name.to_string())
}

fn now_epoch() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()
        .context("system time does not fit in i64 seconds")?)
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

fn opencode_auth_from_file(auth_path: &PathBuf) -> Result<String> {
    let raw =
        fs::read_to_string(auth_path).with_context(|| format!("read {}", auth_path.display()))?;
    let auth: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", auth_path.display()))?;
    let openai = auth
        .get("openai")
        .cloned()
        .ok_or_else(|| anyhow!("{} is missing an openai auth entry", auth_path.display()))?;
    let kind = openai
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{}.openai is missing type", auth_path.display()))?;

    match kind {
        "oauth" => {
            for field in ["refresh", "access", "expires"] {
                if openai.get(field).is_none() {
                    return Err(anyhow!("{}.openai is missing {field}", auth_path.display()));
                }
            }
        }
        "api" => {
            if openai.get("key").is_none() {
                return Err(anyhow!("{}.openai is missing key", auth_path.display()));
            }
        }
        other => {
            return Err(anyhow!(
                "{}.openai has unsupported auth type {other}",
                auth_path.display()
            ));
        }
    }

    println!(
        "will seed OpenCode OpenAI auth from {}",
        auth_path.display()
    );
    Ok(serde_json::to_string_pretty(&json!({ "openai": openai }))?)
}

fn load_mom_config() -> Result<MomConfig> {
    let path = match env::var_os("MOM_CONFIG") {
        Some(value) => PathBuf::from(value),
        None => home_dir()?.join(".config").join("mom").join("config.json"),
    };
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "read Agent Mom config {}; create it or set MOM_CONFIG",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parse Agent Mom config {}", path.display()))
}

fn validate_credential_config(config: &MomConfig, credential_mode: CredentialMode) -> Result<()> {
    match credential_mode {
        CredentialMode::VmAuthJson => {
            if config.codex_auth_path.as_os_str().is_empty() {
                bail!("credential_mode vm-auth-json requires codex_auth_path");
            }
        }
        CredentialMode::OpenRouterProxy => {
            let proxy_url = config.credential_proxy_url.as_deref().unwrap_or("").trim();
            if proxy_url.is_empty() {
                bail!("credential_mode openrouter-proxy requires credential_proxy_url");
            }
            if config.credential_proxy_ca_path.is_none() {
                bail!("credential_mode openrouter-proxy requires credential_proxy_ca_path");
            }
        }
    }
    Ok(())
}

fn resolve_required_file(path: &PathBuf, key: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path)?;
    expanded.canonicalize().with_context(|| {
        format!(
            "{key} does not point at a readable file: {}",
            expanded.display()
        )
    })
}

fn expand_tilde(path: &PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path.clone())
}

fn default_hermes_profile() -> String {
    "main".to_string()
}

fn default_opencode_auth_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("opencode")
        .join("auth.json")
}

fn default_hermes_model() -> String {
    "gpt-5.5".to_string()
}

fn default_snapshot_name() -> String {
    "mom-alpine-agent-base".to_string()
}

fn default_credential_mode() -> String {
    "vm-auth-json".to_string()
}

fn default_workspace_cpus() -> u8 {
    1
}

fn default_workspace_memory() -> u64 {
    2048
}

fn default_workspace_volume_quota() -> u32 {
    10240
}

fn default_workspace_idle_timeout() -> u64 {
    1800
}

fn default_workspace_backup_interval() -> u64 {
    900
}

fn hermes_config_yaml(config: &MomConfig) -> String {
    let credential_mode =
        CredentialMode::parse(&config.credential_mode).unwrap_or(CredentialMode::VmAuthJson);
    let model = config_string(&config.hermes_model);
    let provider = match credential_mode {
        CredentialMode::VmAuthJson => "openai-codex",
        CredentialMode::OpenRouterProxy => "openrouter",
    };
    let api_mode = match credential_mode {
        CredentialMode::VmAuthJson => "  api_mode: codex_responses\n",
        CredentialMode::OpenRouterProxy => "",
    };
    let proxy = config
        .credential_proxy_url
        .as_ref()
        .map(|url| {
            format!(
                r#"
env:
  HTTP_PROXY: {}
  HTTPS_PROXY: {}
  ALL_PROXY: {}
  OPENAI_API_KEY: agentmom-proxy
  OPENROUTER_API_KEY: agentmom-proxy
"#,
                config_string(url),
                config_string(url),
                config_string(url)
            )
        })
        .unwrap_or_default();
    format!(
        r#"model:
  provider: {provider}
  default: {model}
{api_mode}terminal:
  backend: local
  cwd: /workspace
  persistent_shell: true
  timeout: 600
approvals:
  mode: off
toolsets:
  - all
{proxy}
"#
    )
}

fn codex_config_toml(config: &MomConfig) -> String {
    let credential_mode =
        CredentialMode::parse(&config.credential_mode).unwrap_or(CredentialMode::VmAuthJson);
    let model = config_string(&config.hermes_model);
    let mut toml = format!(
        r#"model = {model}
approval_policy = "never"
sandbox_mode = "danger-full-access"

[projects."/workspace"]
trust_level = "trusted"
"#
    );
    if credential_mode == CredentialMode::OpenRouterProxy {
        toml.push_str(
            r#"
# Codex subscription auth is intentionally not configured in openrouter-proxy mode.
"#,
        );
    }
    toml
}

fn proxy_env_sh(proxy_url: &str) -> String {
    let proxy_url = shell_quote(proxy_url);
    format!(
        r#"export HTTP_PROXY={proxy_url}
export HTTPS_PROXY={proxy_url}
export ALL_PROXY={proxy_url}
export OPENAI_API_KEY=agentmom-proxy
export OPENROUTER_API_KEY=agentmom-proxy
export NODE_EXTRA_CA_CERTS=/usr/local/share/ca-certificates/agentmom-proxy.crt
export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
"#
    )
}

fn opencode_config_json(config: &MomConfig) -> String {
    let credential_mode =
        CredentialMode::parse(&config.credential_mode).unwrap_or(CredentialMode::VmAuthJson);
    let model = match credential_mode {
        CredentialMode::VmAuthJson => format!("openai/{}", config.hermes_model),
        CredentialMode::OpenRouterProxy => format!("openrouter/{}", config.hermes_model),
    };
    let model = config_string(&model);
    format!(
        r#"{{
  "$schema": "https://opencode.ai/config.json",
  "model": {model},
  "server": {{
    "hostname": "0.0.0.0",
    "port": {OPENCODE_GUEST_PORT}
  }}
}}
"#
    )
}

fn config_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn hermes_soul_md() -> &'static str {
    "You are running inside an isolated Agent Mom microsandbox. Work in /workspace.\n"
}

fn image_label(config: &microsandbox::sandbox::SandboxConfig) -> String {
    format!("{:?}", config.image)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(credential_mode: &str) -> MomConfig {
        MomConfig {
            codex_auth_path: PathBuf::from("/tmp/codex-auth.json"),
            opencode_auth_path: PathBuf::from("/tmp/opencode-auth.json"),
            hermes_profile: "main".to_string(),
            hermes_model: "anthropic/claude-sonnet-4.6".to_string(),
            snapshot_name: "mom-alpine-agent-base".to_string(),
            credential_mode: credential_mode.to_string(),
            credential_proxy_url: Some("http://127.0.0.1:1080".to_string()),
            credential_proxy_ca_path: Some(PathBuf::from("/tmp/agentmom-proxy-ca.crt")),
        }
    }

    #[test]
    fn vm_auth_json_mode_generates_codex_hermes_config() {
        let config = test_config("vm-auth-json");

        assert_eq!(
            CredentialMode::parse(&config.credential_mode).unwrap(),
            CredentialMode::VmAuthJson
        );
        assert!(validate_credential_config(&config, CredentialMode::VmAuthJson).is_ok());

        let hermes = hermes_config_yaml(&config);
        assert!(hermes.contains("provider: openai-codex"));
        assert!(hermes.contains("api_mode: codex_responses"));
    }

    #[test]
    fn openrouter_proxy_mode_generates_proxy_hermes_config() {
        let config = test_config("openrouter-proxy");

        assert_eq!(
            CredentialMode::parse(&config.credential_mode).unwrap(),
            CredentialMode::OpenRouterProxy
        );
        assert!(validate_credential_config(&config, CredentialMode::OpenRouterProxy).is_ok());

        let hermes = hermes_config_yaml(&config);
        assert!(hermes.contains("provider: openrouter"));
        assert!(!hermes.contains("api_mode: codex_responses"));
        assert!(hermes.contains("OPENROUTER_API_KEY: agentmom-proxy"));

        let codex = codex_config_toml(&config);
        assert!(codex.contains("Codex subscription auth is intentionally not configured"));
    }

    #[test]
    fn openrouter_proxy_mode_requires_proxy_config() {
        let mut config = test_config("openrouter-proxy");
        config.credential_proxy_url = None;
        assert!(validate_credential_config(&config, CredentialMode::OpenRouterProxy).is_err());

        config.credential_proxy_url = Some("http://127.0.0.1:1080".to_string());
        config.credential_proxy_ca_path = None;
        assert!(validate_credential_config(&config, CredentialMode::OpenRouterProxy).is_err());
    }
}
