use super::*;

pub(crate) const FLEET_SCHEMA_VERSION: i64 = 1;

const NODE_HAS_CAPACITY_SQL: &str = r#"
max_active_workspaces = 0 OR (
    SELECT COUNT(*)
    FROM workspaces
    WHERE workspaces.node_id = nodes.node_id
      AND workspaces.desired_state = 'running'
) < max_active_workspaces
"#;

fn ready_node_query(select_clause: &str, extra_where: &str, order_limit: &str) -> String {
    format!(
        r#"
SELECT {select_clause}
FROM nodes
WHERE status = 'ready'
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?1
  {extra_where}
  AND ({NODE_HAS_CAPACITY_SQL})
{order_limit}
"#
    )
}

pub(crate) fn ensure_fleet_schema() -> Result<()> {
    let db = fleet_db()?;
    let current = ensure_supported_schema_without_mutation(&db)?;
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS schema_version (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
    name TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    sandbox_name TEXT NOT NULL UNIQUE,
    volume_name TEXT NOT NULL UNIQUE,
    node_id TEXT,
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
    worker_url TEXT,
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
    add_column_if_missing(&db, "workspaces", "node_id", "TEXT")?;
    add_column_if_missing(&db, "nodes", "worker_url", "TEXT")?;
    migrate_fleet_schema(&db, current)?;
    Ok(())
}

fn migrate_fleet_schema(db: &Connection, current: i64) -> Result<()> {
    match current {
        0 => set_schema_version(db, FLEET_SCHEMA_VERSION),
        FLEET_SCHEMA_VERSION => Ok(()),
        other => bail!("unsupported fleet.db schema version {other}"),
    }
}

fn ensure_supported_schema_without_mutation(db: &Connection) -> Result<i64> {
    let current = read_schema_version_without_mutation(db)?;
    if current > FLEET_SCHEMA_VERSION {
        bail!(
            "fleet.db schema version {current} is newer than this binary supports ({FLEET_SCHEMA_VERSION})"
        );
    }
    if current != 0 && current != FLEET_SCHEMA_VERSION {
        bail!("unsupported fleet.db schema version {current}");
    }
    Ok(current)
}

pub(crate) fn current_fleet_schema_version() -> Result<i64> {
    let db = open_existing_fleet_db(true)?;
    read_schema_version_without_mutation(&db)
}

fn read_schema_version_without_mutation(db: &Connection) -> Result<i64> {
    let has_table = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !has_table {
        return Ok(0);
    }
    db.query_row(
        "SELECT version FROM schema_version WHERE id = 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map(|version| version.unwrap_or(0))
    .map_err(Into::into)
}

fn set_schema_version(db: &Connection, version: i64) -> Result<()> {
    db.execute(
        r#"
INSERT INTO schema_version (id, version, updated_at)
VALUES (1, ?1, ?2)
ON CONFLICT(id) DO UPDATE SET
    version = excluded.version,
    updated_at = excluded.updated_at
"#,
        params![version, now_epoch()?],
    )?;
    Ok(())
}

pub(crate) fn workspace_upsert_pending(
    name: &str,
    user_id: &str,
    sandbox_name: &str,
    volume_name: &str,
    assigned_node_id: Option<&str>,
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
    name, user_id, sandbox_name, volume_name, node_id, desired_state, cpus, memory_mib,
    volume_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8, 'creating', ?9, ?10, ?11, NULL, ?11, ?11)
ON CONFLICT(name) DO NOTHING
"#,
        params![
            name,
            user_id,
            sandbox_name,
            volume_name,
            assigned_node_id,
            i64::from(cpus),
            i64::from(memory_mib),
            i64::from(volume_quota_mib),
            i64::try_from(idle_timeout_secs).context("idle timeout too large")?,
            i64::try_from(backup_interval_secs).context("backup interval too large")?,
            now,
        ],
    )?;
    if db.changes() == 0 {
        bail!("workspace already exists: {name}");
    }
    Ok(())
}

pub(crate) fn workspace_get(name: &str) -> Result<WorkspaceRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
       node_id, status, volume_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at
FROM workspaces
WHERE name = ?1
"#,
        params![name],
        workspace_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("workspace not found: {name}"))
}

pub(crate) fn workspace_all() -> Result<Vec<WorkspaceRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
       node_id, status, volume_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at
FROM workspaces
ORDER BY name
"#,
    )?;
    let records = stmt
        .query_map([], workspace_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(records)
}

pub(crate) fn workspaces_for_node(node: &str) -> Result<Vec<WorkspaceRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT name, user_id, sandbox_name, volume_name, desired_state, cpus, memory_mib,
       node_id, status, volume_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at
FROM workspaces
WHERE node_id = ?1 AND status != 'removed'
ORDER BY name
"#,
    )?;
    Ok(stmt
        .query_map(params![node], workspace_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let cpus: i64 = row.get(5)?;
    let memory_mib: i64 = row.get(6)?;
    let volume_quota_mib: i64 = row.get(9)?;
    let idle_timeout_secs: i64 = row.get(10)?;
    let backup_interval_secs: i64 = row.get(11)?;
    Ok(WorkspaceRecord {
        name: row.get(0)?,
        user_id: row.get(1)?,
        sandbox_name: row.get(2)?,
        volume_name: row.get(3)?,
        desired_state: row.get(4)?,
        node_id: row.get(7)?,
        status: row.get(8)?,
        cpus: cpus as u8,
        memory_mib: memory_mib as u32,
        volume_quota_mib: volume_quota_mib as u32,
        idle_timeout_secs: idle_timeout_secs as u64,
        backup_interval_secs: backup_interval_secs as u64,
        last_used_at: row.get(12)?,
        last_backup_at: row.get(13)?,
    })
}

pub(crate) fn workspace_set_desired(name: &str, desired_state: &str) -> Result<()> {
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET desired_state = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, desired_state, now_epoch()?],
    )?;
    Ok(())
}

pub(crate) fn workspace_touch(name: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET last_used_at = ?2, updated_at = ?2 WHERE name = ?1",
        params![name, now],
    )?;
    Ok(())
}

pub(crate) fn workspace_mark_status(name: &str, status: &str) -> Result<()> {
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET status = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, status, now_epoch()?],
    )?;
    Ok(())
}

pub(crate) fn workspace_mark_backup(name: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        "UPDATE workspaces SET last_backup_at = ?2, updated_at = ?2 WHERE name = ?1",
        params![name, now],
    )?;
    Ok(())
}

pub(crate) fn workspace_reassign_for_restore(name: &str, node: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    let changed = db.execute(
        r#"
UPDATE workspaces
SET node_id = ?2,
    status = 'restore-queued',
    updated_at = ?3
WHERE name = ?1
"#,
        params![name, node, now],
    )?;
    if changed == 0 {
        bail!("workspace not found: {name}");
    }
    Ok(())
}

pub(crate) fn workspace_update_from_worker(
    name: &str,
    status: Option<&str>,
    desired_state: Option<&str>,
    touch: bool,
    mark_backup: bool,
) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    if let Some(status) = status {
        db.execute(
            "UPDATE workspaces SET status = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, status, now],
        )?;
    }
    if let Some(desired_state) = desired_state {
        db.execute(
            "UPDATE workspaces SET desired_state = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, desired_state, now],
        )?;
    }
    if touch {
        db.execute(
            "UPDATE workspaces SET last_used_at = ?2, updated_at = ?2 WHERE name = ?1",
            params![name, now],
        )?;
    }
    if mark_backup {
        db.execute(
            "UPDATE workspaces SET last_backup_at = ?2, updated_at = ?2 WHERE name = ?1",
            params![name, now],
        )?;
    }
    Ok(())
}

pub(crate) fn record_workspace_event(
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

pub(crate) fn record_workspace_event_for_node(
    workspace_name: &str,
    node: &str,
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
            node,
            event_type,
            status,
            message,
            serde_json::to_string(&metadata)?,
            now_epoch()?,
        ],
    )?;
    Ok(())
}

pub(crate) fn workspace_events_since(name: &str, since_epoch: i64) -> Result<Vec<WorkspaceEvent>> {
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

pub(crate) fn workspace_recent_events(name: &str, limit: u32) -> Result<Vec<WorkspaceEvent>> {
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

pub(crate) fn workspace_event_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<WorkspaceEvent> {
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

pub(crate) fn record_backup_artifact(
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

pub(crate) fn record_backup_artifact_for_node(
    workspace_name: &str,
    node: &str,
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
            workspace_name,
            node,
            artifact.kind,
            artifact.location,
            status,
            artifact.size_bytes,
            now_epoch()?,
        ],
    )?;
    Ok(id)
}

pub(crate) fn backup_records_for_workspace(name: &str) -> Result<Vec<BackupRecord>> {
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

pub(crate) fn backup_record_get(id: &str) -> Result<BackupRecord> {
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

pub(crate) fn latest_restic_backup(name: &str) -> Result<BackupRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, location, status, size_bytes, created_at
FROM workspace_backups
WHERE workspace_name = ?1 AND kind = 'restic' AND status = 'succeeded'
ORDER BY created_at DESC
LIMIT 1
"#,
        params![name],
        backup_record_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("no restic backup found for workspace {name}"))
}

pub(crate) fn backup_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRecord> {
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

pub(crate) fn create_job(request: CreateJobRequest) -> Result<JobRecord> {
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

pub(crate) fn select_ready_node(requested: Option<&str>) -> Result<String> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let db = fleet_db()?;
    if let Some(node) = requested {
        let exists = db.query_row(
            &ready_node_query("COUNT(*)", "AND node_id = ?2", ""),
            params![stale_cutoff, node],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if !exists {
            bail!("node is not ready: {node}");
        }
        return Ok(node.to_string());
    }
    db.query_row(
        &ready_node_query("node_id", "", "ORDER BY last_seen_at DESC\nLIMIT 1"),
        params![stale_cutoff],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| anyhow!("no ready worker nodes are registered"))
}

pub(crate) fn node_all() -> Result<Vec<NodeRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT node_id, worker_url, cpus, memory_mib, max_active_workspaces,
       disk_reserve_mib, last_seen_at, status
FROM nodes
ORDER BY node_id
"#,
    )?;
    Ok(stmt
        .query_map([], node_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn node_get(node: &str) -> Result<NodeRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT node_id, worker_url, cpus, memory_mib, max_active_workspaces,
       disk_reserve_mib, last_seen_at, status
FROM nodes
WHERE node_id = ?1
"#,
        params![node],
        node_from_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("node not found: {node}"))
}

pub(crate) fn node_set_status(node: &str, status: &str) -> Result<()> {
    if !matches!(
        status,
        "ready" | "cordoned" | "draining" | "maintenance" | "retired"
    ) {
        bail!("invalid node status: {status}");
    }
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let changed = if status == "ready" {
        db.execute(
            "UPDATE nodes SET status = ?2, last_seen_at = 0 WHERE node_id = ?1",
            params![node, status],
        )?
    } else {
        db.execute(
            "UPDATE nodes SET status = ?2 WHERE node_id = ?1",
            params![node, status],
        )?
    };
    if changed == 0 {
        bail!("node not found: {node}");
    }
    Ok(())
}

pub(crate) fn node_allows_worker_reports(node: &str) -> Result<bool> {
    node_has_status(node, &["ready", "cordoned", "draining"])
}

pub(crate) fn node_allows_worker_claims(node: &str) -> Result<bool> {
    node_has_status(node, &["ready", "cordoned"])
}

fn node_has_status(node: &str, allowed: &[&str]) -> Result<bool> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let status: Option<String> = db
        .query_row(
            "SELECT status FROM nodes WHERE node_id = ?1",
            params![node],
            |row| row.get(0),
        )
        .optional()?;
    Ok(status.is_some_and(|status| allowed.iter().any(|allowed| *allowed == status)))
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRecord> {
    let cpus: i64 = row.get(2)?;
    let memory_mib: i64 = row.get(3)?;
    let max_active_workspaces: i64 = row.get(4)?;
    let disk_reserve_mib: i64 = row.get(5)?;
    Ok(NodeRecord {
        node_id: row.get(0)?,
        worker_url: row.get(1)?,
        cpus: cpus as u32,
        memory_mib: memory_mib as u64,
        max_active_workspaces: max_active_workspaces as u32,
        disk_reserve_mib: disk_reserve_mib as u64,
        last_seen_at: row.get(6)?,
        status: row.get(7)?,
    })
}

pub(crate) fn job_get(id: &str) -> Result<JobRecord> {
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

pub(crate) fn claim_job(node: &str) -> Result<Option<JobRecord>> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let db = fleet_db()?;
    requeue_stale_claims(&db, now)?;
    db.execute(
        r#"
UPDATE jobs
SET status = 'claimed', claimed_by = ?1, claimed_at = ?2, updated_at = ?2
WHERE id = (
    SELECT id
    FROM jobs
    WHERE status = 'queued' AND node_id = ?1
      AND EXISTS (
          SELECT 1 FROM nodes
          WHERE nodes.node_id = ?1
            AND nodes.status IN ('ready', 'cordoned')
      )
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

pub(crate) fn complete_job(id: &str, node: &str, status: &str, output: Value) -> Result<JobRecord> {
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

pub(crate) fn mark_job_running(id: &str, node: &str) -> Result<JobRecord> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    let changed = db.execute(
        r#"
UPDATE jobs
SET status = 'running', claimed_at = ?3, updated_at = ?3
WHERE id = ?1 AND claimed_by = ?2 AND status = 'claimed'
"#,
        params![id, node, now],
    )?;
    if changed == 0 {
        bail!("job {id} is not newly claimed by node {node}");
    }
    job_get(id)
}

pub(crate) fn register_node(
    node: &str,
    capacity: &NodeCapacity,
    worker_url: Option<&str>,
) -> Result<()> {
    ensure_fleet_schema()?;
    if let Some(worker_url) = worker_url {
        validate_worker_url(worker_url)?;
    }
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO nodes (
    node_id, worker_url, cpus, memory_mib, max_active_workspaces, disk_reserve_mib, last_seen_at, status
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ready')
ON CONFLICT(node_id) DO UPDATE SET
    worker_url = COALESCE(excluded.worker_url, nodes.worker_url),
    cpus = excluded.cpus,
    memory_mib = excluded.memory_mib,
    max_active_workspaces = excluded.max_active_workspaces,
    disk_reserve_mib = excluded.disk_reserve_mib,
    last_seen_at = excluded.last_seen_at,
    status = CASE
        WHEN nodes.status IN ('offline', 'disabled', 'quarantined', 'cordoned', 'draining', 'maintenance', 'retired') THEN nodes.status
        ELSE excluded.status
    END
"#,
        params![
            node,
            worker_url,
            i64::from(capacity.cpus),
            i64::try_from(capacity.memory_mib).context("memory capacity too large")?,
            i64::from(capacity.max_active_workspaces),
            i64::try_from(capacity.disk_reserve_mib).context("disk reserve too large")?,
            now_epoch()?,
        ],
    )?;
    Ok(())
}

pub(crate) fn node_mark_offline(node: &str) -> Result<()> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.execute(
        "UPDATE nodes SET status = 'offline', last_seen_at = ?2 WHERE node_id = ?1",
        params![node, now_epoch()?],
    )?;
    Ok(())
}

pub(crate) fn node_worker_url(node: &str) -> Result<Option<String>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let url: Option<String> = db
        .query_row(
            "SELECT worker_url FROM nodes WHERE node_id = ?1",
            params![node],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if let Some(url) = &url {
        validate_worker_url(url)?;
    }
    Ok(url)
}

fn validate_worker_url(worker_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(worker_url)
        .with_context(|| format!("parse worker_url {worker_url:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("worker_url must use http or https: {worker_url}");
    }
    if url.port().is_none() {
        bail!("worker_url must include an explicit port: {worker_url}");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("worker_url must include a host: {worker_url}"))?;
    if host.eq_ignore_ascii_case("localhost") && env::var("MOM_RUNTIME").as_deref() != Ok("fake") {
        bail!("worker_url may not use localhost outside fake runtime: {worker_url}");
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        if addr.is_unspecified() {
            bail!("worker_url may not use an unspecified host: {worker_url}");
        }
        if addr.is_loopback() && env::var("MOM_RUNTIME").as_deref() != Ok("fake") {
            bail!("worker_url may not use loopback host outside fake runtime: {worker_url}");
        }
    }
    if let Ok(allowlist) = env::var("MOM_WORKER_URL_ALLOWLIST") {
        let worker_url = normalized_url(worker_url);
        let allowed = allowlist
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(normalized_url)
            .any(|allowed| allowed == worker_url);
        if !allowed {
            bail!("worker_url is not in MOM_WORKER_URL_ALLOWLIST: {worker_url}");
        }
    }
    Ok(())
}

fn normalized_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn requeue_stale_claims(db: &Connection, now: i64) -> Result<()> {
    let timeout = env_u64("MOM_JOB_CLAIM_TIMEOUT_SECS", 1800);
    if timeout == 0 {
        return Ok(());
    }
    let cutoff = now.saturating_sub(i64::try_from(timeout).context("job claim timeout too large")?);
    db.execute(
        r#"
UPDATE jobs
SET status = 'queued', claimed_by = NULL, claimed_at = NULL, updated_at = ?1
WHERE status IN ('claimed', 'running') AND claimed_at IS NOT NULL AND claimed_at < ?2
"#,
        params![now, cutoff],
    )?;
    Ok(())
}

pub(crate) fn job_counts() -> Result<Vec<(String, i64)>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare("SELECT status, COUNT(*) FROM jobs GROUP BY status")?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn workspace_status_counts() -> Result<Vec<(String, i64)>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare("SELECT status, COUNT(*) FROM workspaces GROUP BY status")?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn node_status_counts() -> Result<Vec<(String, i64)>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare("SELECT status, COUNT(*) FROM nodes GROUP BY status")?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn stale_node_count(stale_cutoff: i64) -> Result<i64> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    Ok(db.query_row(
        "SELECT COUNT(*) FROM nodes WHERE status != 'retired' AND last_seen_at < ?1",
        params![stale_cutoff],
        |row| row.get(0),
    )?)
}

pub(crate) fn oldest_queued_job_age(now: i64) -> Result<i64> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let created_at = db
        .query_row(
            "SELECT MIN(created_at) FROM jobs WHERE status = 'queued'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(now);
    Ok(now.saturating_sub(created_at))
}

#[derive(Debug)]
pub(crate) struct MonitorSnapshot {
    pub(crate) ready_nodes: i64,
    pub(crate) stale_nodes: i64,
    pub(crate) oldest_queued_job_age: i64,
    pub(crate) recent_failed_jobs: i64,
}

pub(crate) fn monitor_snapshot(
    stale_cutoff: i64,
    now: i64,
    failed_since: i64,
) -> Result<MonitorSnapshot> {
    let db = open_existing_fleet_db(true)?;
    let ready_nodes = db.query_row(
        r#"
SELECT COUNT(*)
FROM nodes
WHERE status = 'ready'
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?1
"#,
        params![stale_cutoff],
        |row| row.get(0),
    )?;
    let stale_nodes = db.query_row(
        "SELECT COUNT(*) FROM nodes WHERE status != 'retired' AND last_seen_at < ?1",
        params![stale_cutoff],
        |row| row.get(0),
    )?;
    let oldest_created_at = db
        .query_row(
            "SELECT MIN(created_at) FROM jobs WHERE status = 'queued'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(now);
    let recent_failed_jobs = db.query_row(
        "SELECT COUNT(*) FROM jobs WHERE status = 'failed' AND updated_at >= ?1",
        params![failed_since],
        |row| row.get(0),
    )?;
    Ok(MonitorSnapshot {
        ready_nodes,
        stale_nodes,
        oldest_queued_job_age: now.saturating_sub(oldest_created_at),
        recent_failed_jobs,
    })
}

pub(crate) fn backup_count() -> Result<i64> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    Ok(
        db.query_row("SELECT COUNT(*) FROM workspace_backups", [], |row| {
            row.get(0)
        })?,
    )
}

pub(crate) fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
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

pub(crate) fn backup_due(workspace: &WorkspaceRecord, now: i64) -> bool {
    if workspace.backup_interval_secs == 0 {
        return false;
    }
    match workspace.last_backup_at {
        Some(last) => now.saturating_sub(last) >= workspace.backup_interval_secs as i64,
        None => true,
    }
}

pub(crate) fn fleet_db() -> Result<Connection> {
    let dir = fleet_state_dir()?;
    fs::create_dir_all(&dir)?;
    let db = Connection::open(fleet_db_path()?)?;
    ensure_supported_schema_without_mutation(&db)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    Ok(db)
}

fn open_existing_fleet_db(read_only: bool) -> Result<Connection> {
    let path = fleet_db_path()?;
    if !path.exists() {
        bail!(
            "fleet catalog does not exist at {}; start mom api or set MOM_STATE_DIR to the deployed state directory",
            path.display()
        );
    }
    let db = if read_only {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        Connection::open(path)?
    };
    ensure_supported_schema_without_mutation(&db)?;
    Ok(db)
}

fn fleet_db_path() -> Result<PathBuf> {
    Ok(fleet_state_dir()?.join("fleet.db"))
}

pub(crate) fn backup_fleet_catalog(output: Option<&Path>) -> Result<PathBuf> {
    let path = match output {
        Some(path) => expand_tilde(&path.to_path_buf())?,
        None => default_catalog_backup_path()?,
    };
    if path.exists() {
        bail!(
            "refusing to overwrite existing catalog backup: {}",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create catalog backup directory {}", parent.display()))?;
    }
    let db = open_existing_fleet_db(false)?;
    let path_sql = path.to_string_lossy().into_owned();
    db.execute("VACUUM main INTO ?1", params![path_sql])?;
    Ok(path)
}

fn default_catalog_backup_path() -> Result<PathBuf> {
    Ok(fleet_state_dir()?
        .join("catalog-backups")
        .join(format!("fleet-{}.db", now_epoch()?)))
}

fn add_column_if_missing(
    db: &Connection,
    table: &str,
    column: &str,
    column_type: &str,
) -> Result<()> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|name| name == column);
    if !exists {
        db.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}"),
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn fleet_state_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".local").join("state").join("mom"))
}

pub(crate) fn microsandbox_volume_path(volume_name: &str) -> Result<PathBuf> {
    Ok(microsandbox_home()?.join("volumes").join(volume_name))
}

pub(crate) fn microsandbox_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MSB_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".microsandbox"))
}

pub(crate) fn node_id() -> Result<String> {
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

pub(crate) fn log_record(level: &str, event: &str, workspace: Option<&str>, message: &str) {
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

pub(crate) fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn require_worker_token(headers: &HeaderMap) -> Result<()> {
    let expected = worker_token()?;
    if expected.trim().is_empty() {
        bail!("worker token is empty");
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

pub(crate) fn worker_token() -> Result<String> {
    if let Ok(value) = env::var("MOM_WORKER_TOKEN") {
        return Ok(value);
    }
    if let Some(path) = env::var_os("MOM_WORKER_TOKEN_FILE") {
        return Ok(fs::read_to_string(PathBuf::from(path))?.trim().to_string());
    }
    bail!("worker token is not configured")
}

pub(crate) fn new_id(prefix: &str) -> Result<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{prefix}-{nanos}-{}", std::process::id()))
}

pub(crate) fn escape_metric_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn url_component(value: &str) -> String {
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
