use super::*;

pub(crate) async fn api(args: ApiArgs) -> Result<()> {
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
        .route("/api/workspaces/{name}/move", post(api_move_workspace))
        .route(
            "/api/workspaces/{name}/recover",
            post(api_recover_workspace),
        )
        .route("/api/workspaces/{name}/events", get(api_workspace_events))
        .route("/worker/register", post(api_worker_register))
        .route("/worker/heartbeat", post(api_worker_register))
        .route("/worker/claim", post(api_worker_claim))
        .route("/worker/jobs/{id}/events", post(api_worker_job_event))
        .route("/worker/jobs/{id}/complete", post(api_worker_job_complete))
        .route(
            "/worker/workspaces/{name}/state",
            post(api_worker_workspace_state),
        )
        .route(
            "/worker/workspaces/{name}/events",
            post(api_worker_workspace_event),
        )
        .route(
            "/worker/workspaces/{name}/backups",
            post(api_worker_backup_artifact),
        )
        .route("/worker/events", get(api_worker_events))
        .merge(ui::api_routes())
        .with_state(Arc::new(state));
    let app = ui::serve_assets(app);
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
    Json(mut request): Json<CreateJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let workspace = workspace_get(&request.workspace_name)?;
    match (&request.node_id, &workspace.node_id) {
        (Some(requested), Some(assigned)) if requested != assigned => {
            return Err(ApiError::Anyhow(anyhow!(
                "workspace {} is assigned to node {}, not {}",
                workspace.name,
                assigned,
                requested
            )));
        }
        (None, Some(assigned)) => request.node_id = Some(assigned.clone()),
        (Some(requested), None) => {
            select_ready_node(Some(requested))?;
        }
        (None, None) => {
            return Err(ApiError::Anyhow(anyhow!(
                "workspace {} does not have an assigned node",
                workspace.name
            )));
        }
        _ => {}
    }
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
    if workspace_get(&name).is_ok() {
        return Err(ApiError::Anyhow(anyhow!(
            "workspace already exists: {name}"
        )));
    }
    let assigned_node = select_ready_node(request.node_id.as_deref())?;
    let user_id = request.user.clone().unwrap_or_else(|| name.clone());
    let memory = u32::try_from(request.memory).context("memory must fit in u32 MiB")?;
    workspace_upsert_pending(
        &name,
        &user_id,
        &format!("mom-{name}"),
        &format!("mom-{name}-workspace"),
        Some(&assigned_node),
        request.cpus,
        memory,
        request.volume_quota,
        request.idle_timeout,
        request.backup_interval,
    )?;
    let job = create_job(CreateJobRequest {
        workspace_name: name,
        kind: "create".to_string(),
        node_id: Some(assigned_node),
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

async fn api_move_workspace(
    State(state): State<Arc<ApiState>>,
    AxumPath(name): AxumPath<String>,
    Json(request): Json<WorkspaceRestoreRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    enqueue_workspace_restore(state, &name, request, "workspace_move_queued").await
}

async fn api_recover_workspace(
    State(state): State<Arc<ApiState>>,
    AxumPath(name): AxumPath<String>,
    Json(request): Json<WorkspaceRestoreRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    enqueue_workspace_restore(state, &name, request, "workspace_recovery_queued").await
}

async fn enqueue_workspace_restore(
    state: Arc<ApiState>,
    name: &str,
    request: WorkspaceRestoreRequest,
    event_type: &str,
) -> Result<Json<JobResponse>, ApiError> {
    let workspace = workspace_get(name)?;
    let target_node = select_ready_node(Some(&request.target_node_id))?;
    if workspace.node_id.as_deref() == Some(target_node.as_str())
        && event_type == "workspace_move_queued"
    {
        return Err(ApiError::Anyhow(anyhow!(
            "workspace {} is already assigned to node {}",
            workspace.name,
            target_node
        )));
    }
    let backup = match request.backup_id {
        Some(id) => backup_record_get(&id)?,
        None => latest_restic_backup(&workspace.name)?,
    };
    if backup.workspace_name != workspace.name {
        return Err(ApiError::Anyhow(anyhow!(
            "backup {} belongs to workspace {}, not {}",
            backup.id,
            backup.workspace_name,
            workspace.name
        )));
    }
    if backup.kind != "restic" {
        return Err(ApiError::Anyhow(anyhow!(
            "restore supports restic artifacts only; backup {} is {}",
            backup.id,
            backup.kind
        )));
    }
    workspace_mark_status(&workspace.name, "restore-queued")?;
    record_workspace_event_for_node(
        &workspace.name,
        &target_node,
        event_type,
        "queued",
        "workspace restore queued on target node",
        json!({
            "backup_id": backup.id,
            "backup_location": backup.location,
            "source_node_id": workspace.node_id,
            "target_node_id": target_node
        }),
    )?;
    let job = create_job(CreateJobRequest {
        workspace_name: workspace.name,
        kind: "restore".to_string(),
        node_id: Some(target_node),
        payload: json!({
            "backup_id": backup.id,
            "backup_kind": backup.kind,
            "backup_location": backup.location,
            "source_node_id": workspace.node_id,
            "target_node_id": request.target_node_id
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
    register_node(
        &request.node_id,
        &request.capacity,
        request.worker_url.as_deref(),
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_claim(
    headers: HeaderMap,
    Json(request): Json<ClaimJobRequest>,
) -> Result<Json<Option<JobRecord>>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    register_node(
        &request.node_id,
        &request.capacity,
        request.worker_url.as_deref(),
    )?;
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
    if job.node_id.as_deref() != Some(&request.node_id) {
        return Err(ApiError::Unauthorized(anyhow!(
            "job {id} is not assigned to node {}",
            request.node_id
        )));
    }
    if request.event_type == "job_running" {
        mark_job_running(&id, &request.node_id)?;
    }
    record_workspace_event_for_node(
        &job.workspace_name,
        &request.node_id,
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
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CompleteJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let node_id = request.node_id;
    let status = request.status;
    let job = complete_job(&id, &node_id, &status, request.output)?;
    if job.kind == "restore" && status == "succeeded" {
        finalize_workspace_restore(&job, &node_id)?;
        let _ = state.notifier.send("job_available".to_string());
    }
    Ok(Json(JobResponse { job }))
}

fn finalize_workspace_restore(job: &JobRecord, node: &str) -> Result<()> {
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    let target = payload
        .get("target_node_id")
        .and_then(Value::as_str)
        .unwrap_or(node);
    if target != node {
        bail!("restore job target node {target} does not match completing node {node}");
    }
    let backup_id = payload
        .get("backup_id")
        .and_then(Value::as_str)
        .unwrap_or("-");
    workspace_assign_node(&job.workspace_name, node, "restored")?;
    record_workspace_event_for_node(
        &job.workspace_name,
        node,
        "workspace_restored",
        "succeeded",
        "workspace restored and assigned to target node",
        json!({ "backup_id": backup_id, "job_id": job.id }),
    )?;
    Ok(())
}

async fn api_worker_workspace_state(
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<WorkerWorkspaceStateRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    require_assigned_worker(&name, &request.node_id)?;
    workspace_update_from_worker(
        &name,
        request.status.as_deref(),
        request.desired_state.as_deref(),
        request.touch,
        request.mark_backup,
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_workspace_event(
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<WorkerWorkspaceEventRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    require_assigned_worker(&name, &request.node_id)?;
    record_workspace_event_for_node(
        &name,
        &request.node_id,
        &request.event_type,
        &request.status,
        &request.message,
        request.metadata,
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_backup_artifact(
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<WorkerBackupArtifactRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    require_assigned_worker(&name, &request.node_id)?;
    let artifact = BackupArtifact {
        kind: request.kind,
        location: request.location,
        size_bytes: request.size_bytes,
    };
    let id = record_backup_artifact_for_node(&name, &request.node_id, &artifact, &request.status)?;
    Ok(Json(json!({ "id": id })))
}

fn require_assigned_worker(workspace_name: &str, node: &str) -> Result<(), ApiError> {
    let workspace = workspace_get(workspace_name)?;
    match workspace.node_id.as_deref() {
        Some(assigned) if assigned == node => Ok(()),
        Some(assigned) => Err(ApiError::Unauthorized(anyhow!(
            "workspace {workspace_name} is assigned to node {assigned}, not {node}"
        ))),
        None => Err(ApiError::Unauthorized(anyhow!(
            "workspace {workspace_name} has no assigned node"
        ))),
    }
}

async fn api_worker_events(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<WorkerEventsQuery>,
) -> Result<
    Sse<impl tokio_stream::Stream<Item = std::result::Result<SseEvent, Infallible>>>,
    ApiError,
> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
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
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
