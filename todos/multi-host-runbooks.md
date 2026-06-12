Living runbook. Keep commands concrete and update after real incidents.

# Multi-Host Agent Mom Runbooks

## API Down

Impact: new user work cannot be queued and workers cannot claim jobs. Running
VMs continue locally.

Checks:

```sh
systemctl status agentmom-api
journalctl -u agentmom-api -n 200 --no-pager
curl -fsS http://127.0.0.1:8080/health/ready
curl -fsS http://127.0.0.1:8080/metrics
```

Actions:

- If SQLite is locked or corrupt, stop `agentmom-api`, copy
  `/var/lib/agentmom/fleet.db*` to an incident directory, then restart.
- If disk is full, follow the disk pressure runbook before restarting.
- If the API was accidentally exposed without worker auth, bind it to localhost
  or configure `MOM_WORKER_TOKEN_FILE` before reopening it.

## Worker Down

Impact: queued jobs for that node wait; existing stopped VMs stay stopped.

Checks:

```sh
systemctl status agentmom-worker
journalctl -u agentmom-worker -n 200 --no-pager
mom node status
curl -fsS http://127.0.0.1:8080/metrics | rg agentmom_jobs
```

Actions:

- Restart the worker after confirming the API is healthy.
- If the worker cannot claim jobs, verify `MOM_API_URL` and bearer token config.
- If capacity is exceeded, stop idle workspaces or raise the configured limit
  only after checking actual memory and disk pressure.

## Backup Failing

Impact: RPO is not being met for affected workspaces.

Checks:

```sh
mom workspace backups <workspace>
mom workspace events <workspace> --since 24h
journalctl -u agentmom-worker -n 300 --no-pager | rg backup
```

Actions:

- For restic failures, validate repository credentials outside Agent Mom.
- Verify free space under `/var/lib/agentmom`.
- Run one manual backup after fixing credentials or disk:

```sh
mom workspace backup <workspace>
mom workspace backups <workspace>
```

## Catalog Backup And Restore Drill

Impact: if the API host is lost, the central workspace/job/node catalog is only
recoverable to the newest SQLite catalog backup.

Checks:

```sh
mom db status
systemctl status agentmom-catalog-backup.timer
systemctl status agentmom-catalog-backup.service
ls -lh /var/lib/agentmom/catalog-backups | tail
```

Actions:

- Run an immediate catalog backup before risky API-host maintenance:

```sh
mom db backup --output /var/lib/agentmom/catalog-backups/fleet-manual-$(date -u +%Y%m%dT%H%M%SZ).db
```

- Drill restore into a temporary state dir instead of overwriting production:

```sh
latest="$(ls -1t /var/lib/agentmom/catalog-backups/fleet-*.db | head -1)"
tmpdir="$(mktemp -d /var/lib/agentmom/catalog-restore-drill.XXXXXX)"
cp "$latest" "$tmpdir/fleet.db"
MOM_STATE_DIR="$tmpdir" mom db status
rm -rf "$tmpdir"
```

- Do not restore over `/var/lib/agentmom/fleet.db` while `agentmom-api` is
  running.

## Monitoring And Alerts

Impact: missed checks hide API downtime, dead workers, stuck queues, or backup
regressions.

Checks:

```sh
mom monitor check --api-url http://127.0.0.1:8080 --min-ready-nodes 1 --max-stale-nodes 0
curl -fsS http://127.0.0.1:8080/metrics
systemctl status agentmom-monitor-check.timer
journalctl -u agentmom-monitor-check -n 100 --no-pager
```

Actions:

- If a host is intentionally removed, retire it with `mom node retire <node>`
  or raise the temporary stale-node threshold in Nix.
- If queued-job age is high, inspect worker health and capacity before retrying
  jobs.
- If recent failed jobs exceed the threshold, inspect workspace events and
  worker logs for the failing job kind.

## Idle Stop And Wake

Impact: idle stop preserves memory and cost, but cold wake must stay fast enough
for interactive messages.

Checks:

```sh
mom workspace inspect <workspace>
mom workspace events <workspace> --since 2h
journalctl -u agentmom-worker -n 300 --no-pager | rg 'idle|start|claim|job_available'
```

Actions:

- User-triggered work should queue a job through the API; SSE is only the
  low-latency nudge, and polling is the fallback.
- If a stopped workspace does not wake, check `agentmom-worker` SSE reconnect
  logs, then verify `POST /worker/claim` succeeds with the worker bearer token.
- Do not lengthen polling to paper over SSE failures; fix SSE or worker auth
  first.

## Rolling Worker Update

Impact: planned host maintenance should avoid new placements and preserve
currently running work.

Steps:

```sh
mom node cordon <node>
mom node inspect <node>
mom node drain <node>
systemctl restart agentmom-worker
mom node uncordon <node>
mom node inspect <node>
```

Notes:

- `cordon` stops new placements but lets assigned work continue.
- `drain` stops job claims for the node; use it once current work is quiet.
- `uncordon` sets the node back to ready but requires a fresh heartbeat before
  scheduling resumes.

## Disk Pressure

Impact: workers refuse new claims once available disk falls below reserve.

Checks:

```sh
mom node status
df -h /var/lib/agentmom
du -sh /var/lib/agentmom/backups /var/lib/agentmom/microsandbox 2>/dev/null
```

Actions:

- Move old local tar backups to object storage or restic/kopia.
- Stop idle workspaces before deleting anything.
- Do not remove named volumes unless the user has a good backup and explicitly
  accepts data loss risk.

## Credential Proxy Or Auth Failure

Impact: sandboxes start but OpenAI-compatible calls fail.

Checks:

```sh
mom workspace events <workspace> --since 2h
journalctl -u agentmom-worker -n 200 --no-pager | rg 'proxy|auth|401|403'
```

Actions:

- Confirm production workspaces use `credential_mode = "openrouter-proxy"`.
- Confirm `credential_proxy_url` points at the proxy URL visible from the guest.
- If a custom CA is configured, confirm it was written into the guest trust store.
- Rotate worker/API bearer tokens through the configured token file, then restart
  `agentmom-api` and `agentmom-worker`.
