Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# Multi-Host Agent Mom Without k3s

## Intent

Run Agent Mom across a handful of Hetzner/Latitude-style hosts without Kubernetes first, while keeping a clean path to k3s later. Users should be able to send a message and get fast response startup, while idle workspaces stay stopped to preserve memory and cost.

## Scope

- Build a central Agent Mom API plus one worker per physical host.
- Keep microsandbox VMs host-local; do not share live VM state across hosts.
- Treat workspace data as durable named volumes plus backup artifacts.
- Use SSE for fast worker notification, with transactional HTTP job claiming as the source of truth.
- Use iron-proxy for OpenRouter credential injection so long-lived provider credentials are not copied into sandboxes.
- Use systemd/NixOS deployment on `mom-ctrl`, `mom-1`, `mom-2`, and similar hosts.
- Keep the architecture compatible with a future k3s wrapper, but do not require k3s now.

Out of scope for this phase:

- Kubernetes operators, CRDs, Helm charts, or CNI/CSI integration.
- Active-active live VM migration.
- Shared network filesystems for writable workspace state.
- Building a general-purpose orchestration platform.

## Approach

- Keep Agent Mom's object model narrow: workspace, node, job, event, backup.
- Run a central API role and one worker role per VM host.
- Use a central API database for multi-host state. Start with SQLite only if there is exactly one API process; move to Postgres when API HA or multiple API writers are required.
- Workers keep local SQLite only for host-local cache/state if needed; central job/workspace ownership lives in the API.
- Workers connect to the API via SSE for wake notifications and claim jobs over normal HTTP.
- Jobs are durable and idempotent. SSE is only a low-latency nudge.
- Backups are taken from workspace volumes using restic.
- Logs are structured and support workspace-level incident review.

## Steps

- [x] Create first single-host fleet foundation.
  - [x] Add workspace records in local SQLite.
  - [x] Mount a microsandbox named volume at `/workspace`.
  - [x] Add `mom daemon` for start/idle-stop/backup reconciliation.
  - [x] Add NixOS module for a systemd worker service.
  - [x] Smoke test locally and on `pika-build` from `/tmp`.

- [~] Make worker identity explicit.
  - [x] Add `MOM_NODE_ID` and persist node ID in workspace events/backups/logs.
  - [x] Add node capacity config for CPU, memory, active workspace limit, and disk reserve.
  - [x] Add `mom node status` with host pressure and active sandbox summary.

- [~] Add durable event trail.
  - [x] Add `workspace_events` table.
  - [~] Record create/start/stop/exec/backup/proxy/restore lifecycle events.
  - [x] Add `mom workspace inspect`.
  - [x] Add `mom workspace events --since`.

- [~] Add production-grade logging and metrics.
  - [x] Add `MOM_LOG_FORMAT=json`.
  - [~] Emit structured logs with workspace, node, sandbox, job, and backup IDs.
  - [x] Add health endpoints: `/health/live` and `/health/ready`.
  - [x] Add Prometheus metrics endpoint.
  - [x] Expand metrics for node status, stale nodes, workspace status, and queued-job age.
  - [x] Add lightweight monitor checks suitable for a systemd timer.
  - [x] Add NixOS module options for bind addresses and log format.

- [~] Integrate iron-proxy for credentials and egress.
  - [x] Support explicit `vm-auth-json` mode that copies Codex/Hermes OAuth auth into the VM for compatibility testing.
  - [x] Support explicit `openrouter-proxy` mode that removes VM auth files and points OpenRouter-capable clients through iron-proxy.
  - [x] Configure OpenRouter secret injection for worker hosts.
  - [x] Mount or bake the iron-proxy CA into sandbox trust when configured.
  - [x] Add NixOS service wiring for iron-proxy next to Agent Mom.
  - [~] Add a smoke test proving the sandbox has no real provider key but can call through the proxy.

- [~] Add central API mode.
  - [x] Add `mom api` service with HTTP endpoints for workspaces, jobs, nodes, and events.
  - [x] Remove local mode as a production path; workers report through API endpoints.
  - [x] Define API database schema for nodes, workspaces, jobs, events, and backups.
  - [x] Add authenticated worker registration and heartbeat.

- [x] Add SSE worker notification.
  - [x] Add worker SSE connection: `GET /worker/events?node_id=...`.
  - [x] Send small `job_available` events only.
  - [x] Add periodic fallback claim/reconcile polling.
  - [x] Add reconnect backoff and heartbeat pings.

- [~] Add transactional job claiming.
  - [x] Add `POST /worker/claim` scoped by node and capacity.
  - [~] Make jobs idempotent and resumable after worker restart.
  - [x] Add job kinds: create, start, execute, codex, hermes, stop, backup, restore, warm.
  - [x] Add job status transitions: queued, claimed, succeeded, failed, canceled.

- [~] Add on-demand wake path for low latency.
  - [x] Incoming user message can create an execute/codex/hermes job and immediately notify the assigned worker.
  - [x] Worker starts the VM only if stopped.
  - [ ] Keep active/recent workspaces warm for an idle window.
  - [ ] Stream job progress/events back to API as soon as possible.
  - [ ] Track wake latency, agent-ready latency, and first-token/first-output latency.

- [~] Add host capacity and overcommit control.
  - [~] Track active sandbox count, memory pressure, swap pressure, disk free, and backup load.
  - [x] Refuse or queue new starts when host pressure crosses thresholds.
  - [ ] Prefer stopping idle workspaces before starting new cold work.
  - [ ] Add warm-pool policy only after measuring cold-start latency.

- [~] Add backups and restore discipline.
  - [x] Record backup artifacts and timestamps centrally.
  - [x] Monitor/report when backup age exceeds RPO through `mom monitor check`.
  - [x] Add `mom workspace backups`.
  - [x] Add `mom workspace restore`.
  - [ ] Make `mom workspace backup` queue a worker job when the workspace is assigned to a remote node.
  - [x] Add explicit SQLite catalog backup/status commands.
  - [x] Add NixOS timer for catalog backups on the API host.
  - [x] Upload API catalog backups to the configured restic repository.
  - [x] Add host-loss recovery drill path with `mom fleet recover-host`.

- [~] Add deployment flow for multiple systemd hosts.
  - [x] Document host bootstrap for `pika-build` style NixOS boxes.
  - [x] Add deploy script or Nix flake app for API and worker rollout.
  - [x] Deploy Nix-native API on `mom-ctrl`.
  - [x] Deploy worker-only hosts `mom-1` and `mom-2`.
  - [x] Serve the embedded UI from the API process; do not run a separate UI daemon.
  - [x] Keep per-host worker config small: node ID, API URL, state dirs, MSB_HOME, proxy config.
  - [x] Add runbooks for worker down, API down, backup failing, disk pressure, and proxy blocked request spikes.

- [~] Production ops integration from `worktrees/codex-production-ops`.
  - [x] Keep current `master` worker trust boundaries: ready-node worker gates and worker URL allowlist.
  - [x] Do not take the older ops-worktree flake packaging changes; current `master` already has newer hermetic microsandbox packaging.
  - [x] Integrate catalog schema/version guard and `mom db backup`.
  - [x] Integrate monitor checks and richer `/metrics`.
  - [x] Integrate node list/inspect/cordon/drain/retire/uncordon commands without weakening offline/quarantine behavior.
  - [x] Integrate ignored real-host test harness.
  - [x] Integrate runbook updates for catalog backup, monitoring, idle wake, and rolling updates.
  - [x] Defer managed skills until the core fleet safety work lands.
  - [x] Add guarded host-loss recovery that requires successful restic backups before workspace reassignment.
  - [x] Make host-loss recovery transactional so all preconditions pass before any workspace is reassigned.
  - [x] Restore restic backups into a temporary directory and swap them into place only after restore succeeds.
  - [x] Harden public `/worker/*` routes so only current worker public IPs can reach them through Caddy.

- [ ] Preserve k3s migration path.
  - [x] Keep all runtime config available via env vars/files.
  - [x] Ensure API and worker run with stdout JSON logs and graceful SIGTERM.
  - [x] Avoid systemd-only behavior in application logic.
  - [x] Keep writable paths explicit and mountable.
  - [ ] Later map API to Deployment and worker to DaemonSet without changing the core protocol.

## Implementation Notes

- A single API scheduler is acceptable initially; the API load is expected to be low relative to VM work.
- SSE is preferred over NATS/Redis for first multi-host notification because it adds no new infrastructure component.
- SSE events are not the queue. The database-backed job table is the source of truth.
- User-triggered work should not use slow interval polling. The worker should receive near-immediate SSE nudges and claim jobs over HTTP.
- Periodic polling remains useful as a safety net for missed SSE notifications and for maintenance reconciliation.
- Keep workspace identity separate from sandbox identity. The volume and central workspace row are durable; the VM is replaceable.
- Support two auth modes while the product path is still unsettled:
  `vm-auth-json` leaks subscription credentials into the VM and is for compatibility testing;
  `openrouter-proxy` keeps provider keys on the host and uses iron-proxy to inject OpenRouter auth.
- Use iron-proxy rather than maintaining a custom API-key credential-injection proxy.
- Defer subscription OAuth/token-broker integration until OpenRouter proxy mode is stable.
- The current worker code is the successor to the original `mom daemon`; production workers report all job, workspace, event, and backup state through the central API.
- `mom-ctrl` runs `agentmom-api.service` and Caddy for `agentmom.xyz`. The API binds to `127.0.0.1:8080` and owns the central SQLite database at `/var/lib/agentmom/fleet.db`.
- `mom-1` and `mom-2` run worker-only Agent Mom services from NixOS. Durable state is `/var/lib/agentmom`, microsandbox state is `/var/lib/agentmom/microsandbox`, and services use Nix-declared config.
- `agentmom.xyz` points at `77.42.80.210`. Public mutating API routes are protected by Basic Auth; worker routes currently bypass Caddy auth so workers can register, poll, and report state.
- The deployed worker registration currently reports `mom-1` as 32 CPUs, 131072 MiB memory, 48 max active workspaces, and `mom-2` as 8 CPUs, 65536 MiB memory, 24 max active workspaces.
- Worker hosts run `agentmom-credential-proxy.service` using iron-proxy. The service has a generated CA under `/var/lib/agentmom/iron-proxy`, listens on `:1080`, and writes `openrouter-proxy` guest config via Agent Mom.
- `openrouter-proxy` workspaces no longer receive raw Codex/Hermes auth files. Worker hosts get the OpenRouter key from agenix-managed NixOS secrets.
- Base snapshots are now credential/proxy agnostic. Normal workspace creates clone the existing tool snapshot and then apply current auth/proxy config, so proxy/config iteration no longer rebuilds Alpine/Codex/Hermes. The measured no-rebuild create on `pika-build` was 9 seconds.
- `mom workspace refresh-config <workspace>` re-applies Codex/Hermes/proxy config to an existing workspace without rebuilding the base snapshot.
- Current API smoke tests cover `/health/live`, `/health/ready`, `/metrics`, workspace job creation, worker registration, transactional claim, and SSE `job_available` notification.
- Worker endpoints support shared bearer-token auth through `MOM_WORKER_TOKEN` or `MOM_WORKER_TOKEN_FILE`; local smoke tests cover 401 without the token and success with the token.
- SSH deployment to `mom-1` uses the Tailscale address `100.81.250.67`. `mom-2` deploys through `mom-1` as a Tailscale ProxyJump to `100.92.189.28`.
- Current remote verification: `mom-ctrl` has active `agentmom-api`, Caddy, monitor timer, and catalog backup timer; `mom-1` and `mom-2` both have active `agentmom-worker` and credential proxy. The control DB shows both worker nodes ready and eligible.
- Step 6 remote QA created one workspace per worker, stopped both, restarted both, backed both up to Cloudflare R2 via restic, and stopped both again. All create/start/stop/backup jobs succeeded and were claimed by the expected worker.
- `~/configs` now references Agent Mom as `path:/Users/justin/code/agentmom-fleet`; the accidental `~/configs/agentmom` source mirror has been removed. The `pika-build` deploy path evaluates locally and copies Nix store paths to the remote build/target host rather than rsyncing application source into configs.
- Operational runbooks live in `todos/multi-host-runbooks.md`.
- Production ops integration pass should prioritize operator safety and test coverage over broad feature import. The old ops worktree is useful reference material, but selected changes must be rebased onto the current trust-boundary and restore semantics.
- Production ops integration imported catalog schema/version checks, SQLite catalog backup, richer metrics, monitor checks, node lifecycle controls, host-loss recovery, an ignored real-host test harness, and runbook updates. Managed skills remain intentionally deferred.
- Live recovery QA on 2026-06-12 found and fixed two restore bugs: target-host restic restores must register the named volume in the local microsandbox DB before sandbox recreation, and stale/unusable sandbox records need replace semantics during recovery recreation. After the fixes, a clean `mom-2 -> mom-1` recovery restored two backed workspaces and returned both nodes to strict monitor health.
- nixbuild.net currently rejects builds because billing/free build time is exhausted. All NixOS hosts now keep nixbuild.net configured but allow `max-jobs = 2` local fallback so deploys do not wedge when the remote builder is unavailable.
- Reviewer-driven hardening on 2026-06-12 added transactional host recovery, safer restic restore swap/rollback behavior, catalog-backup restic upload, backup RPO/failure monitor checks, Caddy IP allowlisting for worker routes, and a narrow `mom-2` credential-proxy bind on `192.168.83.1:1080`.
- Current live QA on 2026-06-12: `mom-ctrl`, `mom-1`, and `mom-2` all switched to the hardened Agent Mom commit; `agentmom.xyz` returns 403 for public `/worker/*`, 401 for public Basic Auth routes, both workers are ready, and strict monitor passes with two ready nodes and zero stale/failed/backup-alert counts.
- Current live QA on 2026-06-12: read-only real-fleet tests passed, mutating real-fleet tests passed against `mom-2`, catalog backup uploaded a restic snapshot, and a fresh `mom-2 -> mom-1` host-loss recovery restored two newly backed workspaces.
- CLI gap found during recovery QA: `mom workspace backup <workspace>` still tries to read local volumes on the invoking host. In distributed mode it should queue a `backup` job through the API for the owning worker.
- CLI gap found during recovery QA: `mom workspace inspect <workspace>` reports local sandbox status from the invoking host, which is misleading on `mom-ctrl` for remote workspaces. Distributed inspect should use worker-reported state or clearly label host-local runtime checks.
