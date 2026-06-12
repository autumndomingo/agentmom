Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# Multi-Host Agent Mom Without k3s

## Intent

Run Agent Mom across a handful of Hetzner/Latitude-style hosts without Kubernetes first, while keeping a clean path to k3s later. Users should be able to send a message and get fast response startup, while idle workspaces stay stopped to preserve memory and cost.

## Scope

- Build a central Agent Mom API plus one worker per physical host.
- Keep microsandbox VMs host-local; do not share live VM state across hosts.
- Treat workspace data as durable named volumes plus backup artifacts.
- Use SSE for fast worker notification, with transactional HTTP job claiming as the source of truth.
- Use iron-proxy for credential injection so long-lived provider credentials are never copied into sandboxes.
- Use systemd/NixOS deployment on `pika-build` and similar hosts.
- Keep the architecture compatible with a future k3s wrapper, but do not require k3s now.

Out of scope for this phase:

- Kubernetes operators, CRDs, Helm charts, or CNI/CSI integration.
- Active-active live VM migration.
- Shared network filesystems for writable workspace state.
- Building a general-purpose orchestration platform.

## Approach

- Keep Agent Mom's object model narrow: workspace, node, job, event, backup.
- Split the current single-host daemon into API and worker roles while preserving a single-process/local mode for development.
- Use a central API database for multi-host state. Start with SQLite only if there is exactly one API process; move to Postgres when API HA or multiple API writers are required.
- Workers keep local SQLite only for host-local cache/state if needed; central job/workspace ownership lives in the API.
- Workers connect to the API via SSE for wake notifications and claim jobs over normal HTTP.
- Jobs are durable and idempotent. SSE is only a low-latency nudge.
- Backups are taken from stopped workspace volumes using restic/kopia or a configured command.
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
  - [x] Add NixOS module options for bind addresses and log format.

- [~] Integrate iron-proxy for credentials and egress.
  - [x] Support explicit `vm-auth-json` mode that copies Codex/Hermes OAuth auth into the VM for compatibility testing.
  - [x] Support explicit `openrouter-proxy` mode that removes VM auth files and points OpenRouter-capable clients through iron-proxy.
  - [~] Configure placeholder secret injection for OpenRouter first.
  - [x] Mount or bake the iron-proxy CA into sandbox trust when configured.
  - [x] Add NixOS service wiring for iron-proxy next to Agent Mom.
  - [~] Add a smoke test proving the sandbox has no real provider key but can call through the proxy.

- [~] Add central API mode.
  - [x] Add `mom api` service with HTTP endpoints for workspaces, jobs, nodes, and events.
  - [ ] Keep single-host mode working without the API for local development.
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
  - [ ] Alert/report when backup age exceeds RPO.
  - [x] Add `mom workspace backups`.
  - [x] Add `mom workspace restore`.
  - [ ] Add restore drill command for random workspace verification.

- [~] Add deployment flow for multiple systemd hosts.
  - [x] Document host bootstrap for `pika-build` style NixOS boxes.
  - [x] Add deploy script or Nix flake app for API and worker rollout.
  - [x] Deploy first Nix-native API plus worker pair to `pika-build`.
  - [x] Deploy `agentmom-ui` on `pika-build` against the local fleet API.
  - [x] Keep per-host worker config small: node ID, API URL, state dirs, MSB_HOME, proxy config.
  - [x] Add runbooks for worker down, API down, backup failing, disk pressure, and proxy blocked request spikes.

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
- The current `mom daemon` is the seed of `agentmom-worker`.
- Current slice adds local operator reporting before central API/SSE: node ID, workspace events, inspect/events commands, node status, and JSON daemon logs.
- `pika-build` is running Agent Mom through the NixOS module, not an imperative user service. Durable state is `/var/lib/agentmom`, microsandbox state is `/var/lib/agentmom/microsandbox`, and the service uses Nix-declared config.
- `pika-build` now runs the split `agentmom-api.service` and `agentmom-worker.service` units from the NixOS config. The API binds to `127.0.0.1:8080`; the local worker points at that API, uses SSE plus fallback polling, and registers as node `pika-build`.
- `pika-build` also runs `agentmom-ui.service` on `127.0.0.1:8787`; it uses `MOM_API_URL=http://127.0.0.1:8080` and can be reached with SSH port forwarding.
- The deployed worker registration currently reports 32 CPUs, 131072 MiB memory, 48 max active workspaces, and 102400 MiB disk reserve.
- `pika-build` runs `agentmom-credential-proxy.service` using iron-proxy. The service has a generated CA under `/var/lib/agentmom/iron-proxy`, listens on `:1080`, and writes `openrouter-proxy` guest config via Agent Mom.
- `openrouter-proxy` workspaces no longer receive raw Codex/Hermes auth files. Provider calls still need a real `/var/lib/agentmom/secrets/openrouter-api-key` wired into the Nix config before the end-to-end provider smoke can pass.
- Base snapshots are now credential/proxy agnostic. Normal workspace creates clone the existing tool snapshot and then apply current auth/proxy config, so proxy/config iteration no longer rebuilds Alpine/Codex/Hermes. The measured no-rebuild create on `pika-build` was 9 seconds.
- `mom workspace refresh-config <workspace>` re-applies Codex/Hermes/proxy config to an existing workspace without rebuilding the base snapshot.
- Current API smoke tests cover `/health/live`, `/health/ready`, `/metrics`, workspace job creation, worker registration, transactional claim, and SSE `job_available` notification.
- Worker endpoints support shared bearer-token auth through `MOM_WORKER_TOKEN` or `MOM_WORKER_TOKEN_FILE`; local smoke tests cover 401 without the token and success with the token.
- SSH deployment to `pika-build` uses the Tailscale address `100.81.250.67` with `HostKeyAlias=65.108.234.158`; public SSH to `65.108.234.158:22` was timing out during this slice.
- Current remote verification: `agentmom-api`, `agentmom-worker`, and `agentmom-ui` are active, `systemctl --failed` is empty, `/health/ready` returns ok, `/metrics` returns Prometheus text, `http://127.0.0.1:8787/` serves HTML, and `http://127.0.0.1:8787/api/vms` returns the fleet workspace list.
- `~/configs` now references Agent Mom as `path:/Users/justin/code/agentmom-fleet`; the accidental `~/configs/agentmom` source mirror has been removed. The `pika-build` deploy path evaluates locally and copies Nix store paths to the remote build/target host rather than rsyncing application source into configs.
- Operational runbooks live in `todos/multi-host-runbooks.md`.
