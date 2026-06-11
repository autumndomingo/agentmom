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

- For restic/kopia failures, validate repository credentials outside Agent Mom.
- For local tar fallback failures, verify free space under `/var/lib/agentmom`.
- Run one manual backup after fixing credentials or disk:

```sh
mom workspace backup <workspace>
mom workspace backups <workspace>
```

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

- Confirm production workspaces use `credential_mode = "proxy"`.
- Confirm `credential_proxy_url` points at the proxy URL visible from the guest.
- If a custom CA is configured, confirm it was written into the guest trust store.
- Rotate worker/API bearer tokens through the configured token file, then restart
  `agentmom-api` and `agentmom-worker`.
