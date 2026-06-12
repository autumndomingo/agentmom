use super::*;

#[derive(Clone)]
struct WorkerState {
    api: WorkerApi,
    services: service::ServiceState,
}

#[derive(Clone)]
struct WorkerApi {
    client: reqwest::Client,
    api_url: String,
    node: String,
}

#[derive(Debug, Deserialize)]
struct OpenServiceRequest {
    workspace_name: String,
    sandbox_name: String,
}

pub(crate) async fn worker(args: WorkerArgs) -> Result<()> {
    let node = node_id()?;
    let client = reqwest::Client::new();
    let api_url = args.api_url.trim_end_matches('/').to_string();
    let worker_api = WorkerApi {
        client: client.clone(),
        api_url: api_url.clone(),
        node: node.clone(),
    };
    let worker_url = args
        .worker_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", args.bind));
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("parse worker bind address {}", args.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind worker HTTP {addr}"))?;
    let state = Arc::new(WorkerState {
        api: worker_api.clone(),
        services: service::ServiceState::default(),
    });
    let worker_http = tokio::spawn(run_worker_http(listener, state));
    register_worker(&client, &api_url, &node, &worker_url).await?;
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(32);
    let sse_client = client.clone();
    let sse_url = api_url.clone();
    let sse_node = node.clone();
    let sse_task = tokio::spawn(async move {
        worker_sse_loop(sse_client, sse_url, sse_node, wake_tx).await;
    });

    log_record("info", "worker_start", None, "Agent Mom worker starting");
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        let claimed = worker_claim_once(&worker_api, &worker_url).await?;
        worker_reconcile_once(&worker_api).await?;
        if args.once {
            sse_task.abort();
            worker_http.abort();
            return Ok(());
        }
        if claimed {
            continue;
        }
        tokio::select! {
            _ = &mut shutdown => {
                log_record("info", "worker_shutdown", None, "Agent Mom worker shutting down");
                sse_task.abort();
                worker_http.abort();
                return Ok(());
            },
            _ = wake_rx.recv() => {},
            _ = tokio::time::sleep(Duration::from_secs(args.interval)) => {},
        }
    }
}

async fn run_worker_http(listener: tokio::net::TcpListener, state: Arc<WorkerState>) -> Result<()> {
    let app = Router::new()
        .route("/worker/health", get(worker_health))
        .route("/worker/services/{service}/open", post(worker_open_service))
        .with_state(state);
    let addr = listener
        .local_addr()
        .context("read worker HTTP listener address")?;
    log_record(
        "info",
        "worker_http_start",
        None,
        &format!("Agent Mom worker HTTP listening on http://{addr}"),
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn worker_health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn worker_open_service(
    State(state): State<Arc<WorkerState>>,
    AxumPath(service): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<OpenServiceRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let workspace = state
        .api
        .workspace(&request.workspace_name)
        .await
        .map_err(ApiError::Anyhow)?;
    if workspace.node_id.as_deref() != Some(&state.api.node) {
        return Err(ApiError::Unauthorized(anyhow!(
            "workspace {} is not assigned to worker {}",
            workspace.name,
            state.api.node
        )));
    }
    if request.sandbox_name != workspace.sandbox_name {
        return Err(ApiError::Unauthorized(anyhow!(
            "service-open sandbox does not match workspace assignment"
        )));
    }
    if fake_runtime_enabled() {
        let url = fake_open_service(&workspace.name, &service)
            .await
            .map_err(ApiError::Anyhow)?;
        return Ok(Json(json!({ "url": url })));
    }
    let url = service::open_workspace_service(
        &state.services,
        &workspace.name,
        &workspace.sandbox_name,
        &service,
    )
    .await?;
    Ok(Json(json!({ "url": url })))
}

async fn register_worker(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    worker_url: &str,
) -> Result<()> {
    client
        .post(format!("{api_url}/worker/register"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
            "capacity": node_capacity(),
            "worker_url": worker_url
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
            let response = client
                .get(url)
                .with_worker_token()
                .send()
                .await?
                .error_for_status()?;
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

async fn worker_claim_once(api: &WorkerApi, worker_url: &str) -> Result<bool> {
    let records = api.workspaces().await?;
    let pressure = node_pressure(&records).await?;
    let response = api
        .client
        .post(format!("{}/worker/claim", api.api_url))
        .with_worker_token()
        .json(&json!({
            "node_id": api.node,
            "capacity": node_capacity(),
            "pressure": pressure,
            "worker_url": worker_url
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<JobRecord>>()
        .await?;
    let Some(job) = response else {
        return Ok(false);
    };
    run_claimed_job(api, job).await?;
    Ok(true)
}

async fn run_claimed_job(api: &WorkerApi, job: JobRecord) -> Result<()> {
    worker_job_event(
        &api.client,
        &api.api_url,
        &api.node,
        &job.id,
        "job_running",
        "running",
        "worker started job",
        json!({ "kind": job.kind }),
    )
    .await?;
    let result = execute_job(api, &job).await;
    match result {
        Ok(output) => {
            api.client
                .post(format!("{}/worker/jobs/{}/complete", api.api_url, job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": api.node,
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
            api.client
                .post(format!("{}/worker/jobs/{}/complete", api.api_url, job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": api.node,
                    "status": "failed",
                    "output": { "error": message }
                }))
                .send()
                .await?
                .error_for_status()?;
            log_record("error", "job_failed", Some(&job.workspace_name), &message);
            Ok(())
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

async fn execute_job(api: &WorkerApi, job: &JobRecord) -> Result<Value> {
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    match job.kind.as_str() {
        "create" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            create_workspace_local(api, &workspace, &payload).await?;
            Ok(json!({ "created": true }))
        }
        "start" | "warm" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(
                &workspace.name,
                Some("starting"),
                Some("running"),
                true,
                false,
            )
            .await?;
            ensure_workspace_running_local(api, &workspace).await?;
            Ok(json!({ "started": true }))
        }
        "stop" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            stop_workspace_local(api, &workspace).await?;
            Ok(json!({ "stopped": true }))
        }
        "backup" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            backup_workspace_local(api, &workspace, false).await?;
            Ok(json!({ "backed_up": true }))
        }
        "restore" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            restore_workspace_local(api, &workspace, &payload).await?;
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
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(&workspace.name, None, None, true, false)
                .await?;
            let sandbox = workspace_running_sandbox_local(api, &workspace).await?;
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        "codex" => {
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("codex job payload requires prompt"))?;
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(&workspace.name, None, None, true, false)
                .await?;
            let sandbox = workspace_running_sandbox_local(api, &workspace).await?;
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
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(&workspace.name, None, None, true, false)
                .await?;
            let sandbox = workspace_running_sandbox_local(api, &workspace).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        other => bail!("unknown job kind: {other}"),
    }
}

async fn worker_reconcile_once(api: &WorkerApi) -> Result<()> {
    let records = api.workspaces().await?;
    let now = now_epoch()?;
    for record in records {
        if record
            .node_id
            .as_deref()
            .is_some_and(|assigned| assigned != api.node)
        {
            continue;
        }
        if let Err(error) = worker_reconcile_workspace(api, &record, now).await {
            log_record(
                "error",
                "workspace_reconcile_failed",
                Some(&record.name),
                "workspace reconciliation failed",
            );
            api.update_workspace(&record.name, Some("error"), None, false, false)
                .await?;
            api.event(
                &record.name,
                "workspace_reconcile_failed",
                "failed",
                &format!("{error:#}"),
                json!({ "sandbox": record.sandbox_name, "volume": record.volume_name }),
            )
            .await?;
            eprintln!("reconcile {} failed: {error:#}", record.name);
        }
    }
    Ok(())
}

async fn worker_reconcile_workspace(
    api: &WorkerApi,
    record: &WorkspaceRecord,
    now: i64,
) -> Result<()> {
    if record.desired_state == "running" {
        ensure_workspace_running_local(api, record).await?;
        if record.idle_timeout_secs > 0
            && now.saturating_sub(record.last_used_at) >= record.idle_timeout_secs as i64
        {
            log_record(
                "info",
                "workspace_idle_stop",
                Some(&record.name),
                "workspace idle timeout reached",
            );
            if let Ok(handle) = Sandbox::get(&record.sandbox_name).await {
                if handle.status() == SandboxStatus::Running
                    || handle.status() == SandboxStatus::Draining
                {
                    handle.stop_with_timeout(Duration::from_secs(10)).await?;
                }
            }
            api.update_workspace(
                &record.name,
                Some("idle-stopped"),
                Some("stopped"),
                false,
                false,
            )
            .await?;
            api.event(
                &record.name,
                "workspace_idle_stopped",
                "succeeded",
                "workspace stopped after idle timeout",
                json!({ "idle_seconds": now.saturating_sub(record.last_used_at) }),
            )
            .await?;
        }
    }

    if backup_due(record, now) {
        if let Err(error) = backup_workspace_local(api, record, false).await {
            log_record(
                "error",
                "workspace_backup_failed",
                Some(&record.name),
                "workspace backup failed",
            );
            api.event(
                &record.name,
                "workspace_backup_failed",
                "failed",
                &format!("{error:#}"),
                json!({}),
            )
            .await?;
            eprintln!("backup {} failed: {error:#}", record.name);
        }
    }
    Ok(())
}

impl WorkerApi {
    async fn workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        self.client
            .get(format!(
                "{}/worker/workspaces?node_id={}",
                self.api_url,
                url_component(&self.node)
            ))
            .with_worker_token()
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<WorkspaceRecord>>()
            .await
            .map_err(Into::into)
    }

    async fn workspace(&self, name: &str) -> Result<WorkspaceRecord> {
        self.workspaces()
            .await?
            .into_iter()
            .find(|workspace| workspace.name == name)
            .ok_or_else(|| anyhow!("workspace not found in API: {name}"))
    }

    async fn update_workspace(
        &self,
        name: &str,
        status: Option<&str>,
        desired_state: Option<&str>,
        touch: bool,
        mark_backup: bool,
    ) -> Result<()> {
        self.client
            .post(format!("{}/worker/workspaces/{name}/state", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
                "status": status,
                "desired_state": desired_state,
                "touch": touch,
                "mark_backup": mark_backup
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn event(
        &self,
        name: &str,
        event_type: &str,
        status: &str,
        message: &str,
        metadata: Value,
    ) -> Result<()> {
        self.client
            .post(format!("{}/worker/workspaces/{name}/events", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
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

    async fn record_backup(&self, name: &str, artifact: &BackupArtifact) -> Result<String> {
        let response = self
            .client
            .post(format!("{}/worker/workspaces/{name}/backups", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
                "kind": artifact.kind,
                "location": artifact.location,
                "status": "succeeded",
                "size_bytes": artifact.size_bytes
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        response
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("backup artifact response did not include id"))
    }
}

async fn create_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    if fake_runtime_enabled() {
        return fake_create_workspace(api, workspace, payload).await;
    }
    api.event(
        &workspace.name,
        "workspace_create_started",
        "running",
        "workspace create requested",
        json!({
            "sandbox": workspace.sandbox_name,
            "volume": workspace.volume_name,
            "cpus": workspace.cpus,
            "memory_mib": workspace.memory_mib,
            "volume_quota_mib": workspace.volume_quota_mib
        }),
    )
    .await?;
    if let Err(error) = create_workspace_sandbox(workspace, false).await {
        api.update_workspace(&workspace.name, Some("create-failed"), None, false, false)
            .await?;
        api.event(
            &workspace.name,
            "workspace_create_failed",
            "failed",
            &format!("{error:#}"),
            json!({ "sandbox": workspace.sandbox_name, "volume": workspace.volume_name }),
        )
        .await?;
        return Err(error);
    }
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_created",
        "succeeded",
        "workspace VM created and stopped with persistent volume",
        json!({
            "sandbox": workspace.sandbox_name,
            "volume": workspace.volume_name,
            "user": payload.get("user")
        }),
    )
    .await?;
    Ok(())
}

async fn stop_workspace_local(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    if fake_runtime_enabled() {
        return fake_stop_workspace(api, workspace).await;
    }
    api.update_workspace(&workspace.name, None, Some("stopped"), false, false)
        .await?;
    if let Ok(handle) = Sandbox::get(&workspace.sandbox_name).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
    }
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_stopped",
        "succeeded",
        "workspace stopped",
        json!({ "sandbox": workspace.sandbox_name }),
    )
    .await?;
    Ok(())
}

async fn ensure_workspace_running_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
) -> Result<()> {
    if fake_runtime_enabled() {
        return fake_start_workspace(api, workspace).await;
    }
    match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) if handle.status() == SandboxStatus::Running => {
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await
        }
        Ok(handle) => {
            api.event(
                &workspace.name,
                "sandbox_starting",
                "running",
                "starting workspace sandbox",
                json!({ "sandbox": workspace.sandbox_name }),
            )
            .await?;
            handle.start_detached().await?;
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await?;
            api.event(
                &workspace.name,
                "sandbox_started",
                "succeeded",
                "workspace sandbox started",
                json!({ "sandbox": workspace.sandbox_name }),
            )
            .await
        }
        Err(error) => {
            api.event(
                &workspace.name,
                "sandbox_recreate_started",
                "running",
                "workspace sandbox missing; recreating it around existing volume",
                json!({ "sandbox": workspace.sandbox_name, "volume": workspace.volume_name }),
            )
            .await?;
            create_workspace_sandbox(workspace, true)
                .await
                .with_context(|| {
                    format!(
                        "workspace {} has no sandbox {}; failed to recreate it after: {error:#}",
                        workspace.name, workspace.sandbox_name
                    )
                })?;
            api.event(
                &workspace.name,
                "sandbox_recreated",
                "succeeded",
                "workspace sandbox recreated around existing volume",
                json!({ "sandbox": workspace.sandbox_name, "volume": workspace.volume_name }),
            )
            .await?;
            let handle = Sandbox::get(&workspace.sandbox_name)
                .await
                .with_context(|| format!("get recreated sandbox '{}'", workspace.sandbox_name))?;
            handle.start_detached().await?;
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await?;
            api.event(
                &workspace.name,
                "sandbox_started",
                "succeeded",
                "workspace sandbox started",
                json!({ "sandbox": workspace.sandbox_name }),
            )
            .await
        }
    }
}

async fn create_workspace_sandbox(workspace: &WorkspaceRecord, replace: bool) -> Result<()> {
    let create_args = CreateArgs {
        name: workspace.sandbox_name.clone(),
        replace,
        cpus: workspace.cpus,
        memory: u64::from(workspace.memory_mib),
        rebuild_snapshot: false,
        no_snapshot: false,
    };
    let mount = WorkspaceMount {
        volume_name: workspace.volume_name.clone(),
        volume_quota_mib: workspace.volume_quota_mib,
        workspace_name: workspace.name.clone(),
    };
    create_sandbox(create_args, Some(mount)).await
}

async fn workspace_running_sandbox_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
) -> Result<Sandbox> {
    if fake_runtime_enabled() {
        bail!("MOM_RUNTIME=fake does not support guest command execution yet");
    }
    match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) => match handle.status() {
            SandboxStatus::Running | SandboxStatus::Draining => handle
                .connect_with_timeout(Duration::from_secs(30))
                .await
                .with_context(|| {
                    format!("connect to running sandbox '{}'", workspace.sandbox_name)
                }),
            SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Paused => {
                api.event(
                    &workspace.name,
                    "sandbox_starting",
                    "running",
                    "starting workspace sandbox",
                    json!({ "sandbox": workspace.sandbox_name }),
                )
                .await?;
                let sandbox = handle
                    .start()
                    .await
                    .with_context(|| format!("start sandbox '{}'", workspace.sandbox_name))?;
                api.update_workspace(&workspace.name, Some("running"), None, false, false)
                    .await?;
                api.event(
                    &workspace.name,
                    "sandbox_started",
                    "succeeded",
                    "workspace sandbox started",
                    json!({ "sandbox": workspace.sandbox_name }),
                )
                .await?;
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

async fn backup_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    leave_stopped: bool,
) -> Result<()> {
    if fake_runtime_enabled() {
        let _ = leave_stopped;
        return fake_backup_workspace(api, workspace).await;
    }
    let was_running = match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) => {
            let running = handle.status() == SandboxStatus::Running
                || handle.status() == SandboxStatus::Draining;
            if running {
                api.event(
                    &workspace.name,
                    "backup_stop_started",
                    "running",
                    "stopping workspace before backup",
                    json!({ "sandbox": workspace.sandbox_name }),
                )
                .await?;
                handle.stop_with_timeout(Duration::from_secs(20)).await?;
                api.update_workspace(&workspace.name, Some("backup-stopped"), None, false, false)
                    .await?;
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
    api.event(
        &workspace.name,
        "workspace_backup_started",
        "running",
        "workspace volume backup started",
        json!({ "volume": workspace.volume_name }),
    )
    .await?;
    let artifact = backup::run_restic_backup(workspace, &volume_path).await?;
    let backup_id = api.record_backup(&workspace.name, &artifact).await?;
    api.update_workspace(&workspace.name, None, None, false, true)
        .await?;
    api.event(
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
    )
    .await?;
    if was_running && !leave_stopped && workspace.desired_state == "running" {
        ensure_workspace_running_local(api, workspace).await?;
    }
    Ok(())
}

async fn restore_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    let backup_id = payload
        .get("backup_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job payload requires backup_id"))?;
    let backup_location = payload
        .get("backup_location")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job payload requires backup_location"))?;
    let desired_state = payload
        .get("desired_state")
        .and_then(Value::as_str)
        .unwrap_or(&workspace.desired_state);
    if fake_runtime_enabled() {
        return fake_restore_workspace(api, workspace, backup_id, backup_location).await;
    }
    if let Ok(handle) = Sandbox::get(&workspace.sandbox_name).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(20)).await?;
        }
    }
    api.update_workspace(&workspace.name, Some("restoring"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_restore_started",
        "running",
        "workspace volume restore started",
        json!({ "backup_id": backup_id, "location": backup_location }),
    )
    .await?;
    ensure_workspace_volume_registered_for_restore(workspace).await?;
    let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
    backup::run_restic_restore(backup_id, backup_location, &volume_path).await?;
    api.update_workspace(
        &workspace.name,
        Some("restored"),
        Some(desired_state),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_restored",
        "succeeded",
        "workspace volume restored from backup",
        json!({
            "backup_id": backup_id,
            "location": backup_location,
            "desired_state": desired_state
        }),
    )
    .await?;
    if desired_state == "running" {
        ensure_workspace_running_local(api, workspace).await?;
    } else if Sandbox::get(&workspace.sandbox_name).await.is_err() {
        create_workspace_sandbox(workspace, true).await?;
        api.update_workspace(
            &workspace.name,
            Some("stopped"),
            Some("stopped"),
            false,
            false,
        )
        .await?;
    } else {
        api.update_workspace(
            &workspace.name,
            Some("stopped"),
            Some("stopped"),
            false,
            false,
        )
        .await?;
    }
    Ok(())
}

async fn ensure_workspace_volume_registered_for_restore(workspace: &WorkspaceRecord) -> Result<()> {
    match Volume::get(&workspace.volume_name).await {
        Ok(_) => Ok(()),
        Err(MicrosandboxError::VolumeNotFound(_)) => {
            let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
            if volume_path.exists() {
                fs::remove_dir_all(&volume_path).with_context(|| {
                    format!(
                        "remove orphaned restored volume path {} before registering volume",
                        volume_path.display()
                    )
                })?;
            }
            Volume::builder(&workspace.volume_name)
                .quota(workspace.volume_quota_mib)
                .label("mom.workspace", &workspace.name)
                .create()
                .await
                .with_context(|| format!("register restored volume {}", workspace.volume_name))?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "inspect restored volume registration for {}",
                workspace.volume_name
            )
        }),
    }
}

fn fake_runtime_enabled() -> bool {
    env::var("MOM_RUNTIME").is_ok_and(|value| value == "fake")
}

fn fake_workspace_dir(workspace: &WorkspaceRecord) -> Result<PathBuf> {
    Ok(microsandbox_home()?.join("fake").join(&workspace.name))
}

async fn fake_create_workspace(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec_pretty(&json!({
            "workspace": workspace.name,
            "sandbox": workspace.sandbox_name,
            "volume": workspace.volume_name,
            "payload": payload
        }))?,
    )?;
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_created",
        "succeeded",
        "fake workspace created",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_start_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("state"), b"running")?;
    api.update_workspace(&workspace.name, Some("running"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "sandbox_started",
        "succeeded",
        "fake workspace started",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_stop_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("state"), b"stopped")?;
    api.update_workspace(
        &workspace.name,
        Some("stopped"),
        Some("stopped"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_stopped",
        "succeeded",
        "fake workspace stopped",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_backup_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let snapshot = format!("fake-{}-{}", workspace.name, now_epoch()?);
    fs::write(dir.join("last-backup"), snapshot.as_bytes())?;
    let artifact = BackupArtifact {
        kind: "restic".to_string(),
        location: format!("fake-restic#{snapshot}"),
        size_bytes: Some(0),
    };
    let backup_id = api.record_backup(&workspace.name, &artifact).await?;
    api.update_workspace(&workspace.name, None, None, false, true)
        .await?;
    api.event(
        &workspace.name,
        "workspace_backup_succeeded",
        "succeeded",
        "fake workspace backup completed",
        json!({ "runtime": "fake", "backup_id": backup_id }),
    )
    .await
}

async fn fake_restore_workspace(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    backup_id: &str,
    backup_location: &str,
) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("restored-from"), backup_location.as_bytes())?;
    fs::write(dir.join("state"), b"running")?;
    api.update_workspace(
        &workspace.name,
        Some("running"),
        Some("running"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_restored",
        "succeeded",
        "fake workspace restored from backup",
        json!({ "runtime": "fake", "backup_id": backup_id, "location": backup_location }),
    )
    .await
}

async fn fake_open_service(workspace_name: &str, service: &str) -> Result<String> {
    let dir = microsandbox_home()?.join("fake").join(workspace_name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join(format!("service-{service}")), b"opened")?;
    let base = env::var("MOM_FAKE_SERVICE_BASE_URL")
        .unwrap_or_else(|_| "http://fake.agentmom.local".to_string());
    Ok(format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        workspace_name,
        service
    ))
}
