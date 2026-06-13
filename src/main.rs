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
    MicrosandboxError, Sandbox, Snapshot, SnapshotDestination, Volume, sandbox::SandboxStatus,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command as TokioCommand;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

const IMAGE: &str = "alpine";
const LABEL_MANAGED: &str = "mom.managed";
const LABEL_VERSION: &str = "mom.version";
const GUEST_HERMES_HOME: &str = "/root/.hermes-agent";
const HERMES_GUEST_PORT: u16 = 9119;
const BASE_BUILDER_NAME: &str = "mom-base-builder";
const BASE_DOCTOR_NAME: &str = "mom-base-doctor";

mod acp;
mod api;
mod auth;
mod backup;
mod config;
mod db;
mod sandbox;
mod service;
mod ui;
mod worker;

pub(crate) use config::*;
pub(crate) use db::*;
pub(crate) use sandbox::*;

#[derive(Debug, Parser)]
#[command(
    name = "mom",
    about = "Agent Mom: workspace control plane for microsandbox agent runtimes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    /// Fleet-level operations for the central API database.
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
    /// Inspect and maintain the central fleet catalog.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Run lightweight health checks for alerting.
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    /// Inspect Agent Mom configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
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
    /// Run Hermes in a workspace VM and update its activity timestamp.
    Hermes {
        name: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Re-apply Hermes/proxy configuration to an existing workspace VM.
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
    /// Ensure the required versioned base snapshot exists and passes doctor.
    EnsureBase {
        /// Rebuild the required base snapshot even if it already exists.
        #[arg(long)]
        rebuild: bool,
    },
    /// List registered fleet nodes from the central catalog.
    List,
    /// Show a registered fleet node from the central catalog.
    Inspect { node: String },
    /// Prevent new workspace placement on a node while allowing existing work.
    Cordon { node: String },
    /// Prevent new placement and worker job claims on a node.
    Drain { node: String },
    /// Mark a drained node as intentionally removed from service.
    Retire { node: String },
    /// Return a cordoned or draining node to normal scheduling.
    Uncordon { node: String },
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Show the local fleet catalog schema version.
    Status,
    /// Back up the SQLite fleet catalog with VACUUM INTO.
    Backup {
        /// Destination .db file. Defaults to MOM_STATE_DIR/catalog-backups/fleet-<epoch>.db.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the configured file and print the redacted effective config.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum FleetCommand {
    /// Reassign workspaces from a lost host and queue restore jobs on a ready host.
    RecoverHost {
        /// Host/node ID that is lost.
        #[arg(long)]
        from: String,
        /// Ready host/node ID that should restore the latest backups.
        #[arg(long)]
        to: String,
        /// Print the planned recovery actions without changing state.
        #[arg(long)]
        dry_run: bool,
    },
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

#[derive(Debug, Subcommand)]
enum MonitorCommand {
    /// Exit non-zero if the API/catalog looks unhealthy.
    Check(MonitorCheckArgs),
}

#[derive(Debug, Args)]
struct MonitorCheckArgs {
    /// Optional API URL to check with /health/ready.
    #[arg(long)]
    api_url: Option<String>,
    /// Minimum fresh ready nodes required.
    #[arg(long, default_value_t = 1)]
    min_ready_nodes: i64,
    /// Maximum stale node count allowed.
    #[arg(long, default_value_t = 0)]
    max_stale_nodes: i64,
    /// Maximum oldest queued job age before alerting.
    #[arg(long, default_value_t = 300)]
    max_queued_age_secs: i64,
    /// Look back this many seconds for failed jobs.
    #[arg(long, default_value_t = 900)]
    failed_job_lookback_secs: i64,
    /// Maximum failed jobs allowed in the lookback window.
    #[arg(long, default_value_t = 0)]
    max_recent_failed_jobs: i64,
    /// Maximum backup age for workspaces with scheduled backups. 0 disables this check.
    #[arg(long, default_value_t = 0)]
    max_backup_age_secs: i64,
    /// Maximum scheduled-backup workspaces older than max backup age.
    #[arg(long, default_value_t = 0)]
    max_stale_scheduled_backups: i64,
    /// Maximum backup failure events allowed in the failed-job lookback window.
    #[arg(long, default_value_t = 0)]
    max_recent_backup_failures: i64,
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
    workspace_id: String,
    name: String,
    slug: String,
    display_name: String,
    user_id: String,
    owner_user_id: Option<i64>,
    agent_name: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
struct NodeRecord {
    node_id: String,
    worker_url: Option<String>,
    cpus: u32,
    memory_mib: u64,
    max_active_workspaces: u32,
    disk_reserve_mib: u64,
    last_seen_at: i64,
    status: String,
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

#[derive(Debug, Deserialize)]
struct WorkerWorkspacesQuery {
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Workspace { command } => workspace_command(command).await,
        Command::Node { command } => node_command(command).await,
        Command::Fleet { command } => fleet_command(command).await,
        Command::Db { command } => db_command(command),
        Command::Monitor { command } => monitor_command(command).await,
        Command::Config { command } => config_command(command),
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
        NodeCommand::EnsureBase { rebuild } => {
            let config = load_mom_config()?;
            sandbox::ensure_base_snapshot_for_deploy(&config, rebuild).await
        }
        NodeCommand::List => node_list(),
        NodeCommand::Inspect { node } => node_inspect(&node),
        NodeCommand::Cordon { node } => node_set_scheduling(&node, "cordoned"),
        NodeCommand::Drain { node } => node_set_scheduling(&node, "draining"),
        NodeCommand::Retire { node } => node_set_scheduling(&node, "retired"),
        NodeCommand::Uncordon { node } => node_set_scheduling(&node, "ready"),
    }
}

fn config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Doctor => {
            let path = config::config_path()?;
            let config = load_mom_config()?;
            config.validate_for_node()?;
            println!("loaded Agent Mom config from {}", path.display());
            println!("{}", serde_json::to_string_pretty(&config.redacted_json())?);
            Ok(())
        }
    }
}

fn node_list() -> Result<()> {
    let nodes = node_all()?;
    println!(
        "{:<20} {:<10} {:<10} {:<8} {:<10} {:<8} {:<12} WORKER_URL",
        "NODE", "STATUS", "ELIGIBLE", "CPUS", "MEM", "MAX", "LAST_SEEN"
    );
    for node in nodes {
        let eligible = node_eligible(&node)?;
        println!(
            "{:<20} {:<10} {:<10} {:<8} {:<10} {:<8} {:<12} {}",
            node.node_id,
            node.status,
            eligible,
            node.cpus,
            format!("{}M", node.memory_mib),
            node.max_active_workspaces,
            node.last_seen_at,
            node.worker_url.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn node_inspect(node: &str) -> Result<()> {
    let node = node_get(node)?;
    println!("Node: {}", node.node_id);
    println!("Status: {}", node.status);
    println!("Worker URL: {}", node.worker_url.as_deref().unwrap_or("-"));
    println!("CPUs: {}", node.cpus);
    println!("Memory: {} MiB", node.memory_mib);
    println!("Max active workspaces: {}", node.max_active_workspaces);
    println!("Disk reserve: {} MiB", node.disk_reserve_mib);
    println!("Last seen: {}", node.last_seen_at);
    println!("Eligible: {}", node_eligible(&node)?);
    Ok(())
}

fn node_set_scheduling(node: &str, status: &str) -> Result<()> {
    node_set_status(node, status)?;
    println!("set node {node} status to {status}");
    Ok(())
}

fn node_eligible(node: &NodeRecord) -> Result<&'static str> {
    if node.status != "ready" || node.worker_url.is_none() {
        return Ok("no");
    }
    let stale_cutoff =
        now_epoch()?.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    if node.last_seen_at < stale_cutoff {
        return Ok("no");
    }
    Ok("yes")
}

fn db_command(command: DbCommand) -> Result<()> {
    match command {
        DbCommand::Status => {
            let version = current_fleet_schema_version()?;
            let db = fleet_state_dir()?.join("fleet.db");
            println!("Catalog: {}", db.display());
            println!("Schema version: {version}");
            Ok(())
        }
        DbCommand::Backup { output } => {
            let path = backup_fleet_catalog(output.as_deref())?;
            println!("Backed up catalog to {}", path.display());
            Ok(())
        }
    }
}

async fn monitor_command(command: MonitorCommand) -> Result<()> {
    match command {
        MonitorCommand::Check(args) => monitor_check(args).await,
    }
}

async fn monitor_check(args: MonitorCheckArgs) -> Result<()> {
    if args.min_ready_nodes < 0
        || args.max_stale_nodes < 0
        || args.max_queued_age_secs < 0
        || args.failed_job_lookback_secs < 0
        || args.max_recent_failed_jobs < 0
        || args.max_backup_age_secs < 0
        || args.max_stale_scheduled_backups < 0
        || args.max_recent_backup_failures < 0
    {
        bail!("monitor thresholds must be non-negative");
    }

    let mut issues = Vec::new();
    if let Some(api_url) = args.api_url.as_deref() {
        let url = format!("{}/health/ready", api_url.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => issues.push(format!("api readiness returned {}", response.status())),
            Err(error) => issues.push(format!("api readiness failed: {error:#}")),
        }
    }

    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let failed_since = now.saturating_sub(args.failed_job_lookback_secs);
    let backup_stale_cutoff = now.saturating_sub(args.max_backup_age_secs);
    let snapshot = monitor_snapshot(stale_cutoff, now, failed_since, backup_stale_cutoff)?;

    if snapshot.ready_nodes < args.min_ready_nodes {
        issues.push(format!(
            "ready nodes {} below minimum {}",
            snapshot.ready_nodes, args.min_ready_nodes
        ));
    }
    if snapshot.stale_nodes > args.max_stale_nodes {
        issues.push(format!(
            "stale nodes {} above maximum {}",
            snapshot.stale_nodes, args.max_stale_nodes
        ));
    }
    if snapshot.oldest_queued_job_age > args.max_queued_age_secs {
        issues.push(format!(
            "oldest queued job age {}s above maximum {}s",
            snapshot.oldest_queued_job_age, args.max_queued_age_secs
        ));
    }
    if snapshot.recent_failed_jobs > args.max_recent_failed_jobs {
        issues.push(format!(
            "failed jobs {} in last {}s above maximum {}",
            snapshot.recent_failed_jobs, args.failed_job_lookback_secs, args.max_recent_failed_jobs
        ));
    }
    if args.max_backup_age_secs > 0
        && snapshot.stale_scheduled_backups > args.max_stale_scheduled_backups
    {
        issues.push(format!(
            "stale scheduled backups {} above maximum {} for max backup age {}s",
            snapshot.stale_scheduled_backups,
            args.max_stale_scheduled_backups,
            args.max_backup_age_secs
        ));
    }
    if snapshot.recent_backup_failures > args.max_recent_backup_failures {
        issues.push(format!(
            "backup failures {} in last {}s above maximum {}",
            snapshot.recent_backup_failures,
            args.failed_job_lookback_secs,
            args.max_recent_backup_failures
        ));
    }

    if issues.is_empty() {
        println!(
            "monitor ok: ready_nodes={} stale_nodes={} oldest_queued_job_age_seconds={} recent_failed_jobs={} stale_scheduled_backups={} recent_backup_failures={}",
            snapshot.ready_nodes,
            snapshot.stale_nodes,
            snapshot.oldest_queued_job_age,
            snapshot.recent_failed_jobs,
            snapshot.stale_scheduled_backups,
            snapshot.recent_backup_failures
        );
        return Ok(());
    }

    for issue in &issues {
        eprintln!("monitor alert: {issue}");
    }
    bail!("monitor check failed with {} issue(s)", issues.len())
}

async fn fleet_command(command: FleetCommand) -> Result<()> {
    match command {
        FleetCommand::RecoverHost { from, to, dry_run } => fleet_recover_host(&from, &to, dry_run),
    }
}

fn fleet_recover_host(from: &str, to: &str, dry_run: bool) -> Result<()> {
    select_ready_node(Some(to))?;
    let workspaces = workspaces_for_node(from)?;
    if workspaces.is_empty() {
        println!("no workspaces assigned to {from}");
        if !dry_run {
            node_mark_offline(from)?;
        }
        return Ok(());
    }

    println!(
        "{} workspace(s) assigned to {from} will be restored on {to}",
        workspaces.len()
    );
    let mut recovery_plan = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let backup = latest_restic_backup(&workspace.name).with_context(|| {
            format!(
                "workspace {} cannot be recovered because it has no successful restic backup",
                workspace.name
            )
        })?;
        println!("{} -> {} using {}", workspace.name, to, backup.id);
        recovery_plan.push((workspace, backup));
    }
    if !dry_run {
        recover_host_with_backups(from, to, &recovery_plan)?;
    }
    Ok(())
}

async fn workspace_create(args: WorkspaceCreateArgs) -> Result<()> {
    let display_name = args.name.trim().to_string();
    let name = workspace_slug_from_name(&args.name)?;
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
        &display_name,
        &user_id,
        None,
        None,
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
        "{:<24} {:<24} {:<16} {:<12} {:<8} {:<8} {:<8} VOLUME",
        "SLUG", "DISPLAY", "NODE", "DESIRED", "CPUS", "MEM", "QUOTA"
    );
    for record in records {
        println!(
            "{:<24} {:<24} {:<16} {:<12} {:<8} {:<8} {:<8} {}",
            record.slug,
            record.display_name,
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
    let local_node = node_id().unwrap_or_else(|_| "-".to_string());
    let runtime_is_local = record
        .node_id
        .as_deref()
        .is_none_or(|assigned| assigned == local_node);
    let sandbox_status = if runtime_is_local {
        match Sandbox::get(&record.sandbox_name).await {
            Ok(handle) => format!("{:?}", handle.status()),
            Err(_) => "missing".to_string(),
        }
    } else {
        format!(
            "not checked locally; assigned to {}",
            record.node_id.as_deref().unwrap_or("-")
        )
    };
    let volume_path = microsandbox_volume_path(&record.volume_name)?;
    let events = workspace_recent_events(name, 5)?;

    println!("Workspace: {}", record.name);
    println!("Workspace ID: {}", record.workspace_id);
    println!("Slug: {}", record.slug);
    println!("Display name: {}", record.display_name);
    println!("User: {}", record.user_id);
    println!("Node: {}", record.node_id.as_deref().unwrap_or("-"));
    println!("Inspecting node: {local_node}");
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
    Auth(auth::AuthError),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Anyhow(error)
    }
}

impl From<auth::AuthError> for ApiError {
    fn from(error: auth::AuthError) -> Self {
        Self::Auth(error)
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
            ApiError::Auth(error) => error.into_response(),
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

pub(crate) fn workspace_slug_from_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("workspace name must not be empty");
    }
    if trimmed.len() > 128 {
        bail!("workspace name must be at most 128 bytes");
    }

    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }

    let hash = short_hash(trimmed, 12);
    if slug.is_empty() {
        return Ok(format!("workspace-{hash}"));
    }

    let normalized_without_hash = slug == trimmed && slug.len() <= 64;
    if normalized_without_hash {
        return Ok(slug);
    }

    let suffix_len = hash.len() + 1;
    if slug.len() + suffix_len > 64 {
        slug.truncate(64 - suffix_len);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    Ok(format!("{slug}-{hash}"))
}

pub(crate) fn workspace_id_from_slug(slug: &str) -> String {
    format!("ws_{}", short_hash(slug, 16))
}

fn short_hash(value: &str, chars: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex[..chars].to_string()
}

fn now_epoch() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()
        .context("system time does not fit in i64 seconds")?)
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
    let model = config_string(config.model());
    let proxy = config
        .credential_proxy_url()
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
  provider: openrouter
  default: {model}
terminal:
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

fn config_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn hermes_soul_md() -> &'static str {
    "You are running inside an isolated Agent Mom microsandbox. Work in /workspace.\n"
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MomConfig {
        MomConfig {
            schema_version: 1,
            runtime: RuntimeConfig {
                snapshot_name: Some("mom-base-testrev".to_string()),
            },
            credentials: CredentialConfig {
                proxy_url: Some("http://127.0.0.1:1080".to_string()),
                proxy_ca_path: Some(PathBuf::from("/tmp/agentmom-proxy-ca.crt")),
            },
            guest: GuestConfig {
                hermes_profile: "main".to_string(),
                model: "anthropic/claude-sonnet-4.6".to_string(),
            },
            auth: AuthConfig {
                secret: Some("test-auth-secret".to_string()),
                secret_file: None,
            },
        }
    }

    #[test]
    fn openrouter_proxy_generates_hermes_config() {
        let config = test_config();

        config.validate_for_guest_config().unwrap();
        let hermes = hermes_config_yaml(&config);
        assert!(hermes.contains("provider: openrouter"));
        assert!(!hermes.contains("api_mode: codex_responses"));
        assert!(hermes.contains("OPENROUTER_API_KEY: agentmom-proxy"));
    }

    #[test]
    fn openrouter_proxy_mode_requires_proxy_config() {
        let mut config = test_config();
        config.credentials.proxy_url = None;
        assert!(config.validate_for_guest_config().is_err());

        config.credentials.proxy_url = Some("http://127.0.0.1:1080".to_string());
        config.credentials.proxy_ca_path = None;
        assert!(config.validate_for_guest_config().is_err());
    }

    #[test]
    fn missing_snapshot_name_is_invalid_for_node() {
        let mut config = test_config();
        config.runtime.snapshot_name = None;
        assert!(config.validate_for_node().is_err());
    }

    #[test]
    fn workspace_slug_keeps_simple_names_readable() {
        assert_eq!(workspace_slug_from_name("justin2").unwrap(), "justin2");
        assert_eq!(
            workspace_slug_from_name("build-agent").unwrap(),
            "build-agent"
        );
    }

    #[test]
    fn workspace_slug_hashes_when_normalization_changes_name() {
        let slug = workspace_slug_from_name("My Bot").unwrap();
        assert!(slug.starts_with("my-bot-"));
        assert_ne!(slug, "my-bot");
        assert_eq!(workspace_slug_from_name("My Bot").unwrap(), slug);
    }

    #[test]
    fn workspace_id_is_deterministic_from_slug() {
        assert_eq!(
            workspace_id_from_slug("justin2"),
            workspace_id_from_slug("justin2")
        );
        assert!(workspace_id_from_slug("justin2").starts_with("ws_"));
    }
}
