use std::collections::HashMap;

use super::*;

pub(crate) const FLEET_SCHEMA_VERSION: i64 = 5;

const NODE_HAS_CAPACITY_SQL: &str = r#"
max_active_workspaces = 0 OR (
    SELECT COUNT(*)
    FROM workspaces
    WHERE workspaces.node_id = nodes.node_id
      AND workspaces.desired_state = 'running'
      AND workspaces.status != 'removed'
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

fn ready_node_for_workspace_query(
    select_clause: &str,
    extra_where: &str,
    order_limit: &str,
) -> String {
    format!(
        r#"
SELECT {select_clause}
FROM nodes
WHERE status = 'ready'
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?1
  {extra_where}
  AND ({NODE_HAS_CAPACITY_SQL})
  AND (
      cpus = 0 OR (
          SELECT COALESCE(SUM(workspaces.cpus), 0)
          FROM workspaces
          WHERE workspaces.node_id = nodes.node_id
            AND workspaces.desired_state = 'running'
            AND workspaces.status != 'removed'
      ) + ?2 <= cpus
  )
  AND (
      memory_mib = 0 OR (
          SELECT COALESCE(SUM(workspaces.memory_mib), 0)
          FROM workspaces
          WHERE workspaces.node_id = nodes.node_id
            AND workspaces.desired_state = 'running'
            AND workspaces.status != 'removed'
      ) + ?3 <= memory_mib
  )
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
    workspace_id TEXT NOT NULL UNIQUE,
    slug TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    user_id TEXT NOT NULL,
    owner_user_id INTEGER,
    agent_name TEXT,
    vm_version TEXT NOT NULL,
    vm_name TEXT NOT NULL UNIQUE,
    workspace_dir_name TEXT NOT NULL UNIQUE,
    node_id TEXT,
    desired_state TEXT NOT NULL,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    workspace_quota_mib INTEGER NOT NULL,
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

CREATE TABLE IF NOT EXISTS service_tunnels (
    hostname TEXT PRIMARY KEY,
    workspace_name TEXT NOT NULL,
    node_id TEXT NOT NULL,
    service TEXT NOT NULL,
    url TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_service_tunnels_workspace
ON service_tunnels (workspace_name, service);
"#,
    )?;
    ensure_workspace_vm_version_column(&db)?;
    if current == 3 {
        reset_auth_schema_for_password_auth(&db)?;
    }
    ensure_auth_schema(&db)?;
    if current == 0 || current < FLEET_SCHEMA_VERSION {
        set_schema_version(&db, FLEET_SCHEMA_VERSION)?;
    }
    Ok(())
}

fn reset_auth_schema_for_password_auth(db: &Connection) -> Result<()> {
    db.execute_batch(
        r#"
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS invite_redemptions;
DROP TABLE IF EXISTS invites;
DROP TABLE IF EXISTS users;
"#,
    )?;
    Ok(())
}

fn ensure_workspace_vm_version_column(db: &Connection) -> Result<()> {
    let has_column = db.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name = 'vm_version'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !has_column {
        db.execute_batch(&format!(
            "ALTER TABLE workspaces ADD COLUMN vm_version TEXT NOT NULL DEFAULT {};",
            sql_string_literal(env!("CARGO_PKG_VERSION"))
        ))?;
    }
    backfill_workspace_vm_versions(db)
}

fn backfill_workspace_vm_versions(db: &Connection) -> Result<()> {
    let mut stmt = db.prepare(
        r#"
SELECT workspace_name, metadata_json
FROM workspace_events
WHERE event_type IN ('workspace_created', 'vm_recreated', 'vm_upgraded')
ORDER BY created_at DESC, id DESC
"#,
    )?;
    let mut versions = HashMap::new();
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (workspace_name, metadata_json) = row?;
        if versions.contains_key(&workspace_name) {
            continue;
        }
        if let Some(version) = metadata_version(&metadata_json) {
            versions.insert(workspace_name, version);
        }
    }
    for (workspace_name, version) in versions {
        db.execute(
            "UPDATE workspaces SET vm_version = ?2 WHERE name = ?1",
            params![workspace_name, version],
        )?;
    }
    Ok(())
}

fn metadata_version(metadata_json: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(metadata_json).ok()?;
    ["mom.version", "version", "agentmom_version"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .or_else(|| value.pointer("/labels/mom.version").and_then(Value::as_str))
        .map(ToString::to_string)
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn ensure_auth_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    full_name TEXT NOT NULL DEFAULT '',
    role TEXT NOT NULL CHECK(role IN ('admin', 'user')),
    invite_id INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_seen_at INTEGER
);

CREATE TABLE IF NOT EXISTS invites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    code TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK(role IN ('admin', 'user')),
    max_uses INTEGER,
    used_count INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    created_by_user_id INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS invite_redemptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    invite_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    redeemed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_workspaces_owner_user
ON workspaces(owner_user_id)
WHERE owner_user_id IS NOT NULL;
"#,
    )?;
    Ok(())
}

fn ensure_supported_schema_without_mutation(db: &Connection) -> Result<i64> {
    let current = read_schema_version_without_mutation(db)?;
    if current == 0 || current == 3 || current == 4 || current == FLEET_SCHEMA_VERSION {
        return Ok(current);
    }
    if current > FLEET_SCHEMA_VERSION {
        bail!(
            "fleet.db schema version {current} is newer than this binary supports ({FLEET_SCHEMA_VERSION})"
        );
    }
    bail!(
        "fleet.db schema version {current} is not supported by this hard-cut microvm runtime; create a fresh catalog"
    );
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
        let user_tables = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if user_tables > 0 {
            bail!(
                "fleet.db has existing tables but no schema_version; create a fresh catalog for the hard-cut microvm runtime"
            );
        }
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

pub(crate) struct WorkspaceUpsert<'a> {
    pub(crate) name: &'a str,
    pub(crate) display_name: &'a str,
    pub(crate) user_id: &'a str,
    pub(crate) owner_user_id: Option<i64>,
    pub(crate) agent_name: Option<&'a str>,
    pub(crate) vm_name: &'a str,
    pub(crate) workspace_dir_name: &'a str,
    pub(crate) assigned_node_id: Option<&'a str>,
    pub(crate) cpus: u8,
    pub(crate) memory_mib: u32,
    pub(crate) workspace_quota_mib: u32,
    pub(crate) idle_timeout_secs: u64,
    pub(crate) backup_interval_secs: u64,
}

pub(crate) fn workspace_upsert_pending(input: WorkspaceUpsert<'_>) -> Result<()> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let db = fleet_db()?;
    insert_workspace_pending(&db, input.assigned_node_id, &input, now)
}

pub(crate) fn workspace_upsert_pending_on_ready_node(
    input: WorkspaceUpsert<'_>,
    requested_node: Option<&str>,
) -> Result<String> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let mut db = fleet_db()?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let assigned_node = if let Some(node) = requested_node {
        let exists = tx.query_row(
            &ready_node_for_workspace_query("COUNT(*)", "AND node_id = ?4", ""),
            params![
                stale_cutoff,
                i64::from(input.cpus),
                i64::from(input.memory_mib),
                node
            ],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if !exists {
            bail!("node is not ready or does not have capacity: {node}");
        }
        node.to_string()
    } else {
        tx.query_row(
            &ready_node_for_workspace_query("node_id", "", "ORDER BY last_seen_at DESC\nLIMIT 1"),
            params![
                stale_cutoff,
                i64::from(input.cpus),
                i64::from(input.memory_mib)
            ],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("no ready worker nodes with capacity are registered"))?
    };
    insert_workspace_pending(&tx, Some(&assigned_node), &input, now)?;
    tx.commit()?;
    Ok(assigned_node)
}

fn insert_workspace_pending(
    db: &Connection,
    assigned_node_id: Option<&str>,
    input: &WorkspaceUpsert<'_>,
    now: i64,
) -> Result<()> {
    let slug = workspace_slug_from_name(input.name)?;
    let workspace_id = workspace_id_from_slug(&slug);
    let changed = db.execute(
        r#"
INSERT INTO workspaces (
    name, workspace_id, slug, display_name, user_id, owner_user_id, agent_name, vm_version, vm_name, workspace_dir_name,
    node_id, desired_state, cpus, memory_mib,
    workspace_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running', ?12, ?13, ?14, 'creating', ?15, ?16, ?17, NULL, ?17, ?17)
ON CONFLICT(name) DO NOTHING
"#,
        params![
            input.name,
            workspace_id,
            slug,
            input.display_name,
            input.user_id,
            input.owner_user_id,
            input.agent_name,
            env!("CARGO_PKG_VERSION"),
            input.vm_name,
            input.workspace_dir_name,
            assigned_node_id,
            i64::from(input.cpus),
            i64::from(input.memory_mib),
            i64::from(input.workspace_quota_mib),
            i64::try_from(input.idle_timeout_secs).context("idle timeout too large")?,
            i64::try_from(input.backup_interval_secs).context("backup interval too large")?,
            now,
        ],
    )?;
    if changed == 0 {
        bail!("workspace already exists: {}", input.name);
    }
    Ok(())
}

pub(crate) fn workspace_get(name: &str) -> Result<WorkspaceRecord> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
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
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
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
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
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
    let cpus: i64 = row.get(8)?;
    let memory_mib: i64 = row.get(9)?;
    let workspace_quota_mib: i64 = row.get(12)?;
    let idle_timeout_secs: i64 = row.get(13)?;
    let backup_interval_secs: i64 = row.get(14)?;
    Ok(WorkspaceRecord {
        workspace_id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        display_name: row.get(3)?,
        user_id: row.get(4)?,
        owner_user_id: row.get(17)?,
        agent_name: row.get(18)?,
        vm_version: row.get(19)?,
        vm_name: row.get(5)?,
        workspace_dir_name: row.get(6)?,
        desired_state: row.get(7)?,
        node_id: row.get(10)?,
        status: row.get(11)?,
        cpus: cpus as u8,
        memory_mib: memory_mib as u32,
        workspace_quota_mib: workspace_quota_mib as u32,
        idle_timeout_secs: idle_timeout_secs as u64,
        backup_interval_secs: backup_interval_secs as u64,
        last_used_at: row.get(15)?,
        last_backup_at: row.get(16)?,
    })
}

pub(crate) fn service_tunnel_upsert(
    workspace_name: &str,
    node_id: &str,
    service: &str,
    url: &str,
) -> Result<()> {
    ensure_fleet_schema()?;
    let parsed = reqwest::Url::parse(url).with_context(|| format!("parse service URL {url}"))?;
    if service_tunnel_uses_path_route(&parsed) {
        return Ok(());
    }
    let hostname = parsed
        .host_str()
        .ok_or_else(|| anyhow!("service URL has no hostname: {url}"))?
        .to_ascii_lowercase();
    let db = fleet_db()?;
    db.execute(
        r#"
DELETE FROM service_tunnels
WHERE workspace_name = ?1 AND service = ?2 AND hostname != ?3
"#,
        params![workspace_name, service, hostname],
    )?;
    db.execute(
        r#"
INSERT INTO service_tunnels (hostname, workspace_name, node_id, service, url, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(hostname) DO UPDATE SET
    workspace_name = excluded.workspace_name,
    node_id = excluded.node_id,
    service = excluded.service,
    url = excluded.url,
    updated_at = excluded.updated_at
"#,
        params![
            hostname,
            workspace_name,
            node_id,
            service,
            url,
            now_epoch()?
        ],
    )?;
    Ok(())
}

fn service_tunnel_uses_path_route(url: &reqwest::Url) -> bool {
    url.host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("agentmom.xyz"))
        && url.path().starts_with("/tunnels/")
}

pub(crate) fn service_tunnel_get(
    workspace_name: &str,
    service: &str,
) -> Result<Option<ServiceTunnelRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT hostname, workspace_name, node_id, service, url, updated_at
FROM service_tunnels
WHERE workspace_name = ?1 AND service = ?2
ORDER BY updated_at DESC
LIMIT 1
"#,
        params![workspace_name, service],
        service_tunnel_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn service_tunnels_for_workspace(
    workspace_name: &str,
    service_prefix: &str,
) -> Result<Vec<ServiceTunnelRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT hostname, workspace_name, node_id, service, url, updated_at
FROM service_tunnels
WHERE workspace_name = ?1 AND service LIKE ?2
ORDER BY updated_at DESC, service ASC
"#,
    )?;
    let like = format!("{service_prefix}%");
    Ok(stmt
        .query_map(params![workspace_name, like], service_tunnel_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn service_tunnel_delete(workspace_name: &str, service: &str) -> Result<bool> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let changed = db.execute(
        "DELETE FROM service_tunnels WHERE workspace_name = ?1 AND service = ?2",
        params![workspace_name, service],
    )?;
    Ok(changed > 0)
}

fn service_tunnel_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServiceTunnelRecord> {
    Ok(ServiceTunnelRecord {
        hostname: row.get(0)?,
        workspace_name: row.get(1)?,
        node_id: row.get(2)?,
        service: row.get(3)?,
        url: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

pub(crate) fn service_tunnel_hostname_registered(hostname: &str) -> Result<bool> {
    ensure_fleet_schema()?;
    let hostname = hostname
        .split(':')
        .next()
        .unwrap_or(hostname)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let db = fleet_db()?;
    let found = db.query_row(
        r#"
SELECT EXISTS (
    SELECT 1
    FROM service_tunnels
    JOIN workspaces ON workspaces.name = service_tunnels.workspace_name
    WHERE service_tunnels.hostname = ?1
      AND workspaces.status != 'removed'
)
"#,
        params![hostname],
        |row| row.get::<_, i64>(0),
    )? != 0;
    Ok(found)
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

pub(crate) fn workspace_mark_vm_version(name: &str, version: &str) -> Result<()> {
    let version = version.trim();
    if version.is_empty() {
        bail!("workspace vm_version must not be empty");
    }
    let db = fleet_db()?;
    let changed = db.execute(
        "UPDATE workspaces SET vm_version = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, version, now_epoch()?],
    )?;
    if changed == 0 {
        bail!("workspace not found: {name}");
    }
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

pub(crate) fn recover_host_with_backups(
    from: &str,
    to: &str,
    items: &[(WorkspaceRecord, BackupRecord)],
) -> Result<()> {
    ensure_fleet_schema()?;
    if from == to {
        bail!("source and target nodes must be different: {from}");
    }
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let mut db = fleet_db()?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let mut current_items = Vec::with_capacity(items.len());
    let mut incoming_running: i64 = 0;
    let mut incoming_cpus: i64 = 0;
    let mut incoming_memory_mib: i64 = 0;
    for (workspace, backup) in items {
        let current: Option<(Option<String>, String)> = tx
            .query_row(
                "SELECT node_id, desired_state FROM workspaces WHERE name = ?1 AND status != 'removed'",
                params![workspace.name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_node, current_desired_state)) = current else {
            bail!("workspace {} is no longer recoverable", workspace.name);
        };
        if current_node.as_deref() != Some(from) {
            bail!(
                "workspace {} is no longer assigned to source node {}",
                workspace.name,
                from
            );
        }
        if current_desired_state == "running" {
            incoming_running = incoming_running
                .checked_add(1)
                .ok_or_else(|| anyhow!("incoming workspace count overflowed"))?;
            incoming_cpus = incoming_cpus
                .checked_add(i64::from(workspace.cpus))
                .ok_or_else(|| anyhow!("incoming CPU count overflowed"))?;
            incoming_memory_mib = incoming_memory_mib
                .checked_add(i64::from(workspace.memory_mib))
                .ok_or_else(|| anyhow!("incoming memory count overflowed"))?;
        }
        current_items.push((workspace, backup, current_desired_state));
    }

    let target_capacity = tx
        .query_row(
            r#"
SELECT max_active_workspaces,
       (
           SELECT COUNT(*)
           FROM workspaces
           WHERE node_id = nodes.node_id
             AND desired_state = 'running'
             AND status != 'removed'
       ) AS active_running,
       cpus,
       (
           SELECT COALESCE(SUM(cpus), 0)
           FROM workspaces
           WHERE node_id = nodes.node_id
             AND desired_state = 'running'
             AND status != 'removed'
       ) AS active_cpus,
       memory_mib,
       (
           SELECT COALESCE(SUM(memory_mib), 0)
           FROM workspaces
           WHERE node_id = nodes.node_id
             AND desired_state = 'running'
             AND status != 'removed'
       ) AS active_memory_mib
FROM nodes
WHERE node_id = ?1
  AND status = 'ready'
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?2
"#,
            params![to, stale_cutoff],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        max_active,
        active_running,
        capacity_cpus,
        active_cpus,
        capacity_memory_mib,
        active_memory_mib,
    )) = target_capacity
    else {
        bail!("target node is not ready: {to}");
    };
    let projected_running = active_running
        .checked_add(incoming_running)
        .ok_or_else(|| anyhow!("target node active workspace count overflowed"))?;
    if max_active > 0 && projected_running > max_active {
        bail!(
            "target node {to} does not have capacity for {incoming_running} recovered running workspace(s): {active_running}/{max_active} active"
        );
    }
    let projected_cpus = active_cpus
        .checked_add(incoming_cpus)
        .ok_or_else(|| anyhow!("target node CPU projection overflowed"))?;
    if capacity_cpus > 0 && projected_cpus > capacity_cpus {
        bail!(
            "target node {to} does not have CPU capacity for recovered running workspace(s): {projected_cpus}/{capacity_cpus} CPU"
        );
    }
    let projected_memory_mib = active_memory_mib
        .checked_add(incoming_memory_mib)
        .ok_or_else(|| anyhow!("target node memory projection overflowed"))?;
    if capacity_memory_mib > 0 && projected_memory_mib > capacity_memory_mib {
        bail!(
            "target node {to} does not have memory capacity for recovered running workspace(s): {projected_memory_mib}/{capacity_memory_mib} MiB"
        );
    }

    for (workspace, backup, desired_state) in current_items {
        let superseded_output = serde_json::to_string(&json!({
            "error": "job superseded by host-loss recovery",
            "from_node": from,
            "to_node": to
        }))?;
        tx.execute(
            r#"
UPDATE jobs
SET status = CASE WHEN status = 'queued' THEN 'canceled' ELSE 'failed' END,
    output_json = ?2,
    updated_at = ?3
WHERE workspace_name = ?1
  AND status IN ('queued', 'claimed', 'running')
"#,
            params![workspace.name, superseded_output, now],
        )?;

        tx.execute(
            r#"
UPDATE workspaces
SET node_id = ?2,
    status = 'restore-queued',
    updated_at = ?3
WHERE name = ?1
"#,
            params![workspace.name, to, now],
        )?;
        tx.execute(
            r#"
INSERT INTO workspace_events (
    workspace_name, node_id, event_type, status, message, metadata_json, created_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#,
            params![
                workspace.name,
                to,
                "workspace_recovery_queued",
                "queued",
                "workspace reassigned for host-loss recovery",
                serde_json::to_string(&json!({
                    "from_node": from,
                    "to_node": to,
                    "backup_id": backup.id,
                    "backup_location": backup.location
                }))?,
                now,
            ],
        )?;
        tx.execute(
            r#"
INSERT INTO jobs (
    id, workspace_name, node_id, kind, status, payload_json, output_json,
    claimed_by, claimed_at, created_at, updated_at
) VALUES (?1, ?2, ?3, 'restore', 'queued', ?4, NULL, NULL, NULL, ?5, ?5)
"#,
            params![
                new_id("job")?,
                workspace.name,
                to,
                serde_json::to_string(&json!({
                    "backup_id": backup.id,
                    "backup_location": backup.location,
                    "backup_workspace_name": backup.workspace_name,
                    "desired_state": desired_state,
                    "from_node": from,
                    "to_node": to
                }))?,
                now,
            ],
        )?;
    }

    tx.execute(
        "UPDATE nodes SET status = 'offline', last_seen_at = ?2 WHERE node_id = ?1",
        params![from, now],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn workspace_update_from_worker(
    name: &str,
    node: &str,
    status: Option<&str>,
    desired_state: Option<&str>,
    touch: bool,
    mark_backup: bool,
) -> Result<()> {
    let now = now_epoch()?;
    let db = fleet_db()?;
    let changed = db.execute(
        r#"
UPDATE workspaces
SET status = COALESCE(?3, status),
    desired_state = COALESCE(?4, desired_state),
    last_used_at = CASE WHEN ?5 THEN ?7 ELSE last_used_at END,
    last_backup_at = CASE WHEN ?6 THEN ?7 ELSE last_backup_at END,
    updated_at = ?7
WHERE name = ?1 AND node_id = ?2
"#,
        params![name, node, status, desired_state, touch, mark_backup, now],
    )?;
    if changed == 0 {
        let workspace = workspace_get(name)?;
        match workspace.node_id.as_deref() {
            Some(assigned) => bail!("workspace {name} is assigned to node {assigned}, not {node}"),
            None => bail!("workspace {name} has no assigned node"),
        }
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

pub(crate) fn require_claimable_node(node: &str) -> Result<()> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let db = fleet_db()?;
    let exists = db.query_row(
        r#"
SELECT COUNT(*)
FROM nodes
WHERE node_id = ?1
  AND status IN ('ready', 'cordoned')
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?2
"#,
        params![node, stale_cutoff],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !exists {
        bail!("node is not accepting jobs: {node}");
    }
    Ok(())
}

pub(crate) fn require_ready_worker_node(node: &str) -> Result<()> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))?);
    let db = fleet_db()?;
    let exists = db.query_row(
        r#"
SELECT COUNT(*)
FROM nodes
WHERE node_id = ?1
  AND status = 'ready'
  AND worker_url IS NOT NULL
  AND last_seen_at >= ?2
"#,
        params![node, stale_cutoff],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !exists {
        bail!("node is not ready for recovery: {node}");
    }
    Ok(())
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

pub(crate) fn node_touch(node: &str) -> Result<()> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let changed = db.execute(
        "UPDATE nodes SET last_seen_at = ?2 WHERE node_id = ?1",
        params![node, now_epoch()?],
    )?;
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

pub(crate) fn claim_job(node: &str, capacity_ok: bool, disk_ok: bool) -> Result<Option<JobRecord>> {
    ensure_fleet_schema()?;
    let now = now_epoch()?;
    let mut db = fleet_db()?;
    let tx = db.transaction()?;
    expire_stale_job_leases(&tx, now)?;
    let job_id: Option<String> = tx
        .query_row(
            r#"
	SELECT id
	FROM jobs
	WHERE status = 'queued' AND node_id = ?1
	  AND (
		      kind IN ('stop', 'remove')
		      OR (
		          ?3
		          AND
		          kind = 'restore'
		          AND EXISTS (
		              SELECT 1
		              FROM workspaces queued_workspace
		              WHERE queued_workspace.name = jobs.workspace_name
		                AND queued_workspace.desired_state != 'running'
		          )
		      )
		      OR (
		          ?2
		          AND EXISTS (
	              SELECT 1
	              FROM nodes
	              JOIN workspaces queued_workspace
	                ON queued_workspace.name = jobs.workspace_name
	              WHERE nodes.node_id = ?1
	                AND (
	                    nodes.max_active_workspaces = 0 OR (
	                        SELECT COUNT(*)
	                        FROM workspaces active
	                        WHERE active.node_id = nodes.node_id
	                          AND active.desired_state = 'running'
	                          AND active.status != 'removed'
	                          AND active.name != queued_workspace.name
	                    ) + 1 <= nodes.max_active_workspaces
	                )
	                AND (
	                    nodes.cpus = 0 OR (
	                        SELECT COALESCE(SUM(active.cpus), 0)
	                        FROM workspaces active
	                        WHERE active.node_id = nodes.node_id
	                          AND active.desired_state = 'running'
	                          AND active.status != 'removed'
	                          AND active.name != queued_workspace.name
	                    ) + queued_workspace.cpus <= nodes.cpus
	                )
	                AND (
	                    nodes.memory_mib = 0 OR (
	                        SELECT COALESCE(SUM(active.memory_mib), 0)
	                        FROM workspaces active
	                        WHERE active.node_id = nodes.node_id
	                          AND active.desired_state = 'running'
	                          AND active.status != 'removed'
	                          AND active.name != queued_workspace.name
	                    ) + queued_workspace.memory_mib <= nodes.memory_mib
	                )
	          )
	      )
	  )
	  AND EXISTS (
	      SELECT 1 FROM nodes
	      WHERE nodes.node_id = ?1
        AND nodes.status IN ('ready', 'cordoned')
  )
  AND NOT EXISTS (
      SELECT 1 FROM jobs active
      WHERE active.workspace_name = jobs.workspace_name
        AND active.status IN ('claimed', 'running')
  )
ORDER BY created_at ASC
	LIMIT 1
	"#,
            params![node, capacity_ok, disk_ok],
            |row| row.get(0),
        )
        .optional()?;
    let Some(job_id) = job_id else {
        tx.commit()?;
        return Ok(None);
    };
    let changed = tx.execute(
        r#"
UPDATE jobs
SET status = 'claimed', claimed_by = ?2, claimed_at = ?3, updated_at = ?3
WHERE id = ?1 AND status = 'queued'
"#,
        params![job_id, node, now],
    )?;
    if changed != 1 {
        bail!("failed to claim queued job {job_id}");
    }
    let job = tx.query_row(
        r#"
SELECT id, workspace_name, node_id, kind, status, payload_json, output_json,
       claimed_by, claimed_at, created_at, updated_at
FROM jobs
WHERE id = ?1
"#,
        params![job_id],
        job_from_row,
    )?;
    tx.commit()?;
    Ok(Some(job))
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
        let job = job_get(id)?;
        if job.claimed_by.as_deref() == Some(node)
            && job.status == status
            && job.output_json.as_deref() == Some(output_json.as_str())
        {
            return Ok(job);
        }
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
WHERE id = ?1 AND claimed_by = ?2 AND status IN ('claimed', 'running')
"#,
        params![id, node, now],
    )?;
    if changed == 0 {
        bail!("job {id} is not claimed by node {node}");
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

    if let Ok(allowlist) = env::var("MOM_WORKER_URL_ALLOWLIST") {
        let worker_url = normalized_url(worker_url);
        let allowed = allowlist
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(normalized_url)
            .any(|allowed| allowed == worker_url);
        if allowed {
            return Ok(());
        }
        bail!("worker_url is not in MOM_WORKER_URL_ALLOWLIST: {worker_url}");
    }

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
    Ok(())
}

fn normalized_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn expire_stale_job_leases(db: &Connection, now: i64) -> Result<()> {
    let claim_timeout = env_u64("MOM_JOB_CLAIM_TIMEOUT_SECS", 1800);
    if claim_timeout != 0 {
        let cutoff = now
            .saturating_sub(i64::try_from(claim_timeout).context("job claim timeout too large")?);
        db.execute(
            r#"
UPDATE jobs
SET status = 'queued', claimed_by = NULL, claimed_at = NULL, updated_at = ?1
WHERE status = 'claimed' AND claimed_at IS NOT NULL AND claimed_at < ?2
"#,
            params![now, cutoff],
        )?;
    }

    let running_timeout = env_u64("MOM_JOB_RUNNING_TIMEOUT_SECS", 1800);
    if running_timeout != 0 {
        let cutoff = now.saturating_sub(
            i64::try_from(running_timeout).context("job running timeout too large")?,
        );
        let output = serde_json::to_string(&json!({
            "error": "job running lease expired before the worker completed it"
        }))?;
        db.execute(
            r#"
UPDATE jobs
SET status = 'failed', output_json = ?3, updated_at = ?1
WHERE status = 'running' AND claimed_at IS NOT NULL AND claimed_at < ?2
"#,
            params![now, cutoff, output],
        )?;
    }
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

pub(crate) fn recent_jobs(limit: u32) -> Result<Vec<JobRecord>> {
    ensure_fleet_schema()?;
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT id, workspace_name, node_id, kind, status, payload_json, output_json,
       claimed_by, claimed_at, created_at, updated_at
FROM jobs
ORDER BY updated_at DESC, created_at DESC
LIMIT ?1
"#,
    )?;
    Ok(stmt
        .query_map(params![i64::from(limit)], job_from_row)?
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
    pub(crate) stale_scheduled_backups: i64,
    pub(crate) recent_backup_failures: i64,
}

pub(crate) fn monitor_snapshot(
    stale_cutoff: i64,
    now: i64,
    failed_since: i64,
    backup_stale_cutoff: i64,
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
    let stale_scheduled_backups = db.query_row(
        r#"
SELECT COUNT(*)
FROM workspaces
WHERE status != 'removed'
  AND backup_interval_secs > 0
  AND (last_backup_at IS NULL OR last_backup_at < ?1)
"#,
        params![backup_stale_cutoff],
        |row| row.get(0),
    )?;
    let recent_backup_failures = db.query_row(
        r#"
SELECT COUNT(*)
FROM workspace_events
WHERE event_type = 'workspace_backup_failed'
  AND status = 'failed'
  AND created_at >= ?1
"#,
        params![failed_since],
        |row| row.get(0),
    )?;
    Ok(MonitorSnapshot {
        ready_nodes,
        stale_nodes,
        oldest_queued_job_age: now.saturating_sub(oldest_created_at),
        recent_failed_jobs,
        stale_scheduled_backups,
        recent_backup_failures,
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
    db.busy_timeout(std::time::Duration::from_secs(5))?;
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
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_supported_schema_without_mutation(&db)?;
    Ok(db)
}

fn fleet_db_path() -> Result<PathBuf> {
    Ok(fleet_state_dir()?.join("fleet.db"))
}

pub(crate) fn backup_fleet_catalog(output: Option<&Path>) -> Result<PathBuf> {
    let path = match output {
        Some(path) => expand_tilde(path)?,
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

pub(crate) fn fleet_state_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MOM_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".local").join("state").join("mom"))
}

pub(crate) fn node_id() -> Result<String> {
    if let Ok(value) = env::var("MOM_NODE_ID")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    if let Ok(value) = env::var("HOSTNAME")
        && !value.trim().is_empty()
    {
        return Ok(value);
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
    require_bearer_token(headers, &expected)
}

pub(crate) fn require_worker_node_token(headers: &HeaderMap, node: &str) -> Result<()> {
    let tokens = worker_node_tokens()?;
    if tokens.is_empty() {
        return require_worker_token(headers);
    }
    let expected = tokens
        .get(node)
        .ok_or_else(|| anyhow!("worker token is not configured for node {node}"))?;
    require_bearer_token(headers, expected)
        .with_context(|| format!("authenticate worker node {node}"))
}

pub(crate) fn worker_token_for_node(node: &str) -> Result<String> {
    let tokens = worker_node_tokens()?;
    if tokens.is_empty() {
        return worker_token();
    }
    tokens
        .get(node)
        .cloned()
        .ok_or_else(|| anyhow!("worker token is not configured for node {node}"))
}

fn require_bearer_token(headers: &HeaderMap, expected: &str) -> Result<()> {
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

fn worker_node_tokens() -> Result<HashMap<String, String>> {
    let Some(raw) = env::var_os("MOM_WORKER_TOKEN_FILES") else {
        return Ok(HashMap::new());
    };
    let raw = raw.to_string_lossy();
    let mut tokens = HashMap::new();
    for entry in raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let (node, path) = entry
            .split_once('=')
            .ok_or_else(|| anyhow!("MOM_WORKER_TOKEN_FILES entries must be node=path"))?;
        let node = node.trim();
        if node.is_empty() {
            bail!("MOM_WORKER_TOKEN_FILES contains an empty node id");
        }
        let token = fs::read_to_string(PathBuf::from(path.trim()))
            .with_context(|| format!("read worker token file for node {node}: {path}"))?
            .trim()
            .to_string();
        if token.is_empty() {
            bail!("worker token file for node {node} is empty: {path}");
        }
        tokens.insert(node.to_string(), token);
    }
    Ok(tokens)
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
