Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# Hetzner Agent Mom Deployment And Abuse Testing

## Intent

Make the current non-k3s Agent Mom fleet real across `pika-build` plus the
existing `hetzner` host. The target is a boring, repeatable Nix deployment with
enough destructive testing that a second worker host feels safe before we add
more capacity or VPS burst scaling.

## Scope

- Deploy one central API/UI on `pika-build`.
- Deploy workers on `pika-build` and `hetzner`.
- Keep API/UI behind `agentmom.xyz` basic auth and/or Tailscale-only access.
- Keep worker control endpoints private to Tailscale or another private network.
- Use restic as the single durable backup system.
- Treat microsandbox local volumes as host-local cache that can be rebuilt from
  restic once restore/move flows are implemented.
- Build a remote integration test harness that can run against real hosts and
  deliberately break workers, networking, capacity, and backups.

Out of scope for this slice:

- Kubernetes/k3s.
- Live VM migration.
- Automatic VPS provisioning.
- Node-bound worker credentials. Workers are trusted holders of the shared
  worker token for now.

## Current Facts

- `master` includes the Agent Mom fleet worker PR.
- Worktree for this slice: `worktrees/hetzner-deploy-test`.
- `~/configs` is the deployment config checkout for this phase. It should point
  `inputs.agentmom.url` at this worktree while testing, then switch to a stable
  git input after the code stabilizes.
- The landed Agent Mom Nix module requires `services.agentmom.workerTokenFile`
  whenever API or worker is enabled. `pika-build` config does not set it yet.
- `pika-build` already has API, UI, worker, Caddy, iron-proxy, and OpenRouter
  agenix wiring.
- `hetzner` already has Tailscale-only SSH, KVM/microvm support, `/data`
  storage, and no public TCP ports.
- Existing secrets include `openrouter-api-key.age` and local-only `r2.age`.

## Architecture Target

`pika-build`:

- `agentmom-api` bound to localhost.
- UI served by `agentmom-api`.
- Caddy serves `agentmom.xyz` with auth and proxies to the API/UI.
- Local worker registered as node `pika-build`.
- iron-proxy for OpenRouter credential injection.
- restic credentials installed through agenix.

`hetzner`:

- Worker only.
- API URL points at `https://agentmom.xyz` if Caddy auth can support worker
  bearer auth cleanly, otherwise at a Tailscale-only API listener.
- Worker bind and advertised URL are Tailscale/private addresses.
- Same Agent Mom package, microsandbox runtime, snapshot name, worker token,
  credential mode, proxy config, and restic environment as `pika-build`.

Backups:

- Use one restic repository initially.
- Prefer Cloudflare R2 for this phase because it is independent of Hetzner host
  failure and the repo already has an `r2.age` secret pattern. Hetzner Object
  Storage remains a good latency/cost alternative if R2 request cost or latency
  becomes annoying.
- Store server runtime env as an agenix secret, not in Nix store:
  `RESTIC_REPOSITORY`, `RESTIC_PASSWORD`, `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, and `AWS_DEFAULT_REGION=auto` for R2.
- Use per-environment bucket/prefix naming, for example
  `agentmom-prod/workspaces`.

## Steps

- [x] Pull latest `master`.
- [x] Create worktree `worktrees/hetzner-deploy-test`.
- [x] Read `todos/test.md` as reference only.
- [x] Inspect active `hetzner` and `pika-build` Nix host shapes.

- [ ] Config deployment lane.
  - [x] Commit/checkpoint `~/configs` before Agent Mom deployment edits.
  - [x] Point `~/configs` Agent Mom input at this worktree for testing, or at
        `github:autumndomingo/agentmom` after the code stabilizes.
  - [x] Add a shared agenix `agentmom-worker-token` secret.
  - [x] Add a shared agenix `agentmom-restic-env` secret.
  - [x] Wire `workerTokenFile` on `pika-build`.
  - [x] Add first-class Agent Mom module option for worker restic env files.
  - [x] Wire restic env into `agentmom-worker` on `pika-build`.
  - [x] Add `inputs.agentmom.nixosModules.agentmom` to `hetzner`.
  - [x] Configure `hetzner` as worker-only with private bind/url.
  - [x] Open firewall only for private worker control traffic.
  - [x] Evaluate both NixOS configs before switching.
  - [ ] Switch `pika-build`, then `hetzner`.

- [ ] Real-host testing lane.
  - [x] Build a test runner that reads host/API parameters from env.
  - [ ] Confirm both nodes register and expose expected capacity.
  - [ ] Create workspaces on automatic placement and explicit node placement.
  - [ ] Verify node capacity rejection when a target is full.
  - [ ] Verify jobs stay pinned to the owning node.
  - [ ] Verify worker endpoints reject missing/wrong token.
  - [ ] Verify service-open routes to the owning worker.
  - [ ] Run OpenRouter proxy smoke: no provider key in the VM, provider call
        works through iron-proxy.
  - [ ] Run backup smoke: write marker, backup, list restic snapshots, inspect
        Agent Mom backup artifact.
  - [ ] Kill/restart worker during queued, claimed, running, backup, and
        service-open scenarios.
  - [ ] Simulate stale worker by stopping one node and verify new work avoids it.
  - [ ] Load test burst creates and user-message jobs across both nodes.
  - [ ] Record latency: job enqueue to claim, claim to VM running, prompt to
        first output.

- [ ] Restore/move design lane.
  - [ ] Define explicit workspace ownership transfer states.
  - [ ] Define restore job payload from a restic artifact.
  - [ ] Require restore drills before claiming host-loss recovery.
  - [ ] Avoid deleting old local copies until the target restore is verified.
  - [ ] Add CLI/API commands for move, recover, drain, and lost-node handling.

- [ ] Idle and wake lane.
  - [ ] Keep `desired_state` as the intent and VM runtime as disposable.
  - [ ] Stop microsandboxes after idle timeout but keep workspace volume.
  - [ ] Wake on user message by queuing a job and using SSE to nudge the owning
        worker immediately.
  - [ ] For cron, do not rely on the VM being awake. Store schedules centrally
        or in the host worker, enqueue jobs at due time, and let the worker start
        the VM before running the task.
  - [ ] Track cold wake latency separately from warm job latency.
  - [ ] Add a grace window for recently active users before stopping the VM.

- [ ] Burst scaling exploration.
  - [ ] Keep the current scheduler provider-neutral: node rows, capacity,
        last-seen, and worker URL are enough for a manually added box.
  - [ ] Design a `node pool` concept before adding provider APIs.
  - [ ] Evaluate Hetzner Cloud first for burst workers because the main fleet is
        already Hetzner-adjacent; compare Latitude only if GPU/bare-metal
        availability matters.
  - [ ] Define bootstrap contract for an ephemeral worker: provision, join
        Tailscale, receive agenix/secrets or one-time worker token, register,
        drain, backup/cleanup, destroy.
  - [ ] Set a policy threshold: use burst VPS for temporary overflow, add a
        permanent Hetzner host when sustained demand crosses the box limit.

## Testing Principles

- Prefer real-host tests for deployment confidence; keep local fake tests for
  fast protocol regressions.
- Every destructive test must write a unique marker and verify no data loss or
  split ownership afterward.
- Test both API behavior and host state: database rows, service status, logs,
  microsandbox files, and restic snapshots.
- Capture commands and expected outputs in the plan as we learn them.
- Do not count a backup system as working until a restore drill passes.
- Real-host tests are ignored by default and require explicit env flags before
  creating workspaces or running backups.

## Open Questions

- Should the worker API use `agentmom.xyz` through Caddy or a separate
  Tailscale-only API bind? Tailscale-only is cleaner for worker traffic.
- Should restic use Cloudflare R2 first or Hetzner Object Storage first? My
  default is R2 for provider-independent disaster recovery.
- Does the current Agent Mom Nix module need first-class `resticEnvFile`, or
  should configs override `systemd.services.agentmom-worker.serviceConfig`
  directly for the first deploy?
- Do we want cron definitions in Agent Mom DB, or should user agents create
  scheduled jobs through a separate scheduler service?

## Agent Work Split

- Worker A: real-host test harness and destructive test matrix.
- Worker B: Nix configs for `pika-build` + `hetzner`, secrets, restic, private
  networking.
- Worker C: design specs for burst workers plus idle/cron wake semantics.

## Implementation Notes

- Added `services.agentmom.worker.resticEnvFile` so restic/S3 credentials can be
  injected through agenix or another runtime secret file without entering the
  Nix store.
- Added ignored real-host integration tests in `tests/real_fleet.rs` and a
  `just real-fleet-test` shortcut.
- Created Cloudflare R2 bucket `agentmom-backups`, encrypted restic S3 env into
  `~/configs/secrets/agentmom-restic-env.age`, initialized the restic repository
  under `prod/workspaces`, and verified a local smoke backup.
- `~/configs` commits:
  - `bb47ad4 wire agentmom fleet worker token`
  - `bd8eb8f wire agentmom restic r2 backups`
- Real-host test env contract:
  `AGENTMOM_REAL_API_URL`, `AGENTMOM_REAL_WORKER_TOKEN`,
  optional `AGENTMOM_REAL_BASIC_AUTH=user:password`,
  `AGENTMOM_REAL_NODE_A`, `AGENTMOM_REAL_NODE_B`,
  `AGENTMOM_REAL_ALLOW_CREATE=1`, and
  `AGENTMOM_REAL_ALLOW_BACKUP=1`.
- Normal fake fleet tests occasionally exceeded the old 15s process-start
  deadline under parallel load in a cold worktree. The shared test wait deadline
  is now 45s; the successful rerun still completed in about 1.3s.
