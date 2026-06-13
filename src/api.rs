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
        .route("/worker/workspaces", get(api_worker_workspaces))
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
    let workspace_statuses = workspace_status_counts()?;
    let jobs = job_counts()?;
    let backups = backup_count()?;
    let node_statuses = node_status_counts()?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(
        i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))
            .context("MOM_NODE_STALE_SECS is too large")?,
    );
    let stale_nodes = stale_node_count(stale_cutoff)?;
    let queued_age = oldest_queued_job_age(now)?;
    Ok(format!(
        "# HELP agentmom_workspaces Total workspaces in the Agent Mom database\n\
         # TYPE agentmom_workspaces gauge\n\
         agentmom_workspaces {workspaces}\n\
         # HELP agentmom_workspaces_by_status Workspaces by status\n\
         # TYPE agentmom_workspaces_by_status gauge\n\
{}\
         # HELP agentmom_nodes Nodes by status\n\
         # TYPE agentmom_nodes gauge\n\
{}\
         # HELP agentmom_nodes_stale Nodes whose last heartbeat is older than MOM_NODE_STALE_SECS\n\
         # TYPE agentmom_nodes_stale gauge\n\
         agentmom_nodes_stale {stale_nodes}\n\
         # HELP agentmom_backups_total Backup artifact records\n\
         # TYPE agentmom_backups_total gauge\n\
         agentmom_backups_total {backups}\n\
         # HELP agentmom_oldest_queued_job_age_seconds Age of the oldest queued job, or 0 when none are queued\n\
         # TYPE agentmom_oldest_queued_job_age_seconds gauge\n\
         agentmom_oldest_queued_job_age_seconds {queued_age}\n\
         # HELP agentmom_jobs Jobs by status\n\
         # TYPE agentmom_jobs gauge\n{}",
        workspace_statuses
            .into_iter()
            .map(|(status, count)| format!(
                "agentmom_workspaces_by_status{{status=\"{}\"}} {}\n",
                escape_metric_label(&status),
                count
            ))
            .collect::<String>(),
        node_statuses
            .into_iter()
            .map(|(status, count)| format!(
                "agentmom_nodes{{status=\"{}\"}} {}\n",
                escape_metric_label(&status),
                count
            ))
            .collect::<String>(),
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
    let display_name = request.name.trim().to_string();
    let name = workspace_slug_from_name(&request.name)?;
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
        &display_name,
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
    if !node_allows_worker_claims(&request.node_id)? {
        require_worker_report_allowed(&request.node_id)?;
        return Ok(Json(None));
    }
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
    require_worker_report_allowed(&request.node_id)?;
    if request.event_type == "job_running" {
        mark_job_running(&id, &request.node_id)?;
    }
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
    require_worker_report_allowed(&request.node_id)?;
    let job = complete_job(&id, &request.node_id, &request.status, request.output)?;
    Ok(Json(JobResponse { job }))
}

async fn api_worker_workspace_state(
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<WorkerWorkspaceStateRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    require_assigned_worker(&name, &request.node_id)?;
    require_worker_report_allowed(&request.node_id)?;
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
    require_worker_report_allowed(&request.node_id)?;
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
    require_worker_report_allowed(&request.node_id)?;
    let artifact = BackupArtifact {
        kind: request.kind,
        location: request.location,
        size_bytes: request.size_bytes,
    };
    let id = record_backup_artifact_for_node(&name, &request.node_id, &artifact, &request.status)?;
    Ok(Json(json!({ "id": id })))
}

async fn api_worker_workspaces(
    headers: HeaderMap,
    Query(query): Query<WorkerWorkspacesQuery>,
) -> Result<Json<Vec<WorkspaceRecord>>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    require_worker_report_allowed(&query.node_id)?;
    Ok(Json(workspaces_for_node(&query.node_id)?))
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

fn require_worker_report_allowed(node: &str) -> Result<(), ApiError> {
    if node_allows_worker_reports(node)? {
        return Ok(());
    }
    Err(ApiError::Unauthorized(anyhow!(
        "node {node} is not allowed to report worker state"
    )))
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
    require_worker_report_allowed(&node_id)?;
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
