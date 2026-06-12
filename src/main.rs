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
const HERMES_GUEST_PORT: u16 = 9119;
const BASE_BUILDER_NAME: &str = "mom-base-builder";

mod api;
mod backup;
mod db;
mod sandbox;
mod service;
mod ui;
mod worker;

pub(crate) use db::*;
pub(crate) use sandbox::*;

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
    #[command(alias = "ws")]
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
    /// Restore a workspace from a restic backup artifact.
    Restore {
        name: String,
        /// Backup artifact ID. Defaults to the latest restic backup.
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
    /// Back up at most this often. Set 0 to disable worker-scheduled backups.
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
    /// HTTP bind address for worker-local control endpoints.
    #[arg(long, default_value = "127.0.0.1:9090", env = "MOM_WORKER_BIND")]
    bind: String,
    /// URL the central API should use to reach this worker.
    #[arg(long, env = "MOM_WORKER_URL")]
    worker_url: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRecord {
    name: String,
    user_id: String,
    sandbox_name: String,
    volume_name: String,
    node_id: Option<String>,
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
    #[serde(default)]
    worker_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimJobRequest {
    node_id: String,
    capacity: NodeCapacity,
    pressure: NodePressure,
    #[serde(default)]
    worker_url: Option<String>,
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
    pub(crate) kind: String,
    pub(crate) location: String,
    pub(crate) size_bytes: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct WorkerWorkspaceStateRequest {
    node_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    desired_state: Option<String>,
    #[serde(default)]
    touch: bool,
    #[serde(default)]
    mark_backup: bool,
}

#[derive(Debug, Deserialize)]
struct WorkerWorkspaceEventRequest {
    node_id: String,
    event_type: String,
    status: String,
    message: String,
    #[serde(default)]
    metadata: Value,
}

#[derive(Debug, Deserialize)]
struct WorkerBackupArtifactRequest {
    node_id: String,
    kind: String,
    location: String,
    status: String,
    #[serde(default)]
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
        Command::Api(args) => api::api(args).await,
        Command::Worker(args) => worker::worker(args).await,
    }
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
            backup::backup_workspace(&workspace, leave_stopped).await
        }
        WorkspaceCommand::Backups { name } => backup::workspace_backups(&name),
        WorkspaceCommand::Restore { name, backup_id } => {
            backup::workspace_restore(&name, backup_id.as_deref()).await
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
        Some(&node_id()?),
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
        "{:<24} {:<16} {:<16} {:<12} {:<8} {:<8} {:<8} VOLUME",
        "WORKSPACE", "USER", "NODE", "DESIRED", "CPUS", "MEM", "QUOTA"
    );
    for record in records {
        println!(
            "{:<24} {:<16} {:<16} {:<12} {:<8} {:<8} {:<8} {}",
            record.name,
            record.user_id,
            record.node_id.as_deref().unwrap_or("-"),
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
    println!("Node: {}", record.node_id.as_deref().unwrap_or("-"));
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
    0
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
