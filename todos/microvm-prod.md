Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# MicroVM Prod Hardening

## Intent

Make the hard-cut microvm.nix runtime deployable to production with old Agent Mom data wiped. Prefer fewer runtime knobs, stronger startup validation, and declarative Nix/systemd behavior.

## Scope

Do:
- Fix proxy, bridge, locked-input, runtime validation, and local/remote routing issues.
- Remove unsupported or unused configuration surface.
- Simplify workspace VM creation and fake-runtime wiring where it is low risk.
- Continue fast-start/resume work on a branch after the prod cutover is stable.

Do not:
- Preserve microsandbox compatibility.
- Preserve old fleet DB data or migrations.
- Land warm pools or snapshot/restore to `master` before they are proven.

## Approach

Keep the first production runtime narrow: Cloud Hypervisor, `/24` guest network, systemd-managed bridge, generated workspace flakes pinned to the deployment flake inputs, and assignment-based worker routing.

## Steps

- [x] Fix prod-deploy blockers from review.
- [x] Remove unsupported Nix module knobs and over-flexible network config.
- [x] Tighten runtime validation for real host readiness.
- [x] Route backup/restore by assigned node before filesystem presence.
- [x] Simplify workspace VM creation API.
- [x] Simplify schema/version handling for wiped prod data.
- [x] Simplify fake runtime dispatch if it stays low-risk.
- [x] Update docs/dev scripts and rerun Rust/Nix validation.

## Implementation Notes

- Old prod data will be wiped, so rejecting old DB versions is acceptable.
- Cloud Hypervisor is the only supported hypervisor for this deployment target.
- Guest network remains `/24` until there is real IPAM.
- Credential proxy listen URL now derives from the microVM bridge host address.
- `mom node ensure-runtime` is intentionally expensive: it builds a host-check runner so deploys fail before accepting work.
- Remote backup/restore now routes by assigned node before looking at local workspace directories.
- Empty catalogs require the configured bootstrap admin code for first-admin creation.
- Multi-worker API deployments use `workerNodeTokenFiles` so worker bearer tokens are node-scoped.
- Prod hosts use a fresh `cutoverWipeMarker` for each destructive cutover; reusing a marker that already exists in `stateDir` is a no-op.

## Second Review Before Deploy

- [x] Replace hash-based VM IP/tap/MAC assignment with a persisted unique lease.
- [x] Fix worker permissions for systemd-managed VM lifecycle and host workspace ownership.
- [x] Make restore non-destructive until restic restore succeeds and validate backup/workspace matches.
- [x] Stop blind requeue of long-running `running` jobs with a narrower retry policy.
- [x] Require local assignment for CLI lifecycle operations or enqueue remote worker jobs.
- [x] Tighten guest isolation and load required host tap/vhost modules.
- [x] Make generated workspace VM flakes build from deployed closure inputs, not network-dependent GitHub locks at start time.
- [x] Add worker URL allowlist/private URL to prod examples and update `mom-1`/`mom-2` host configs from microsandbox options.
- [x] Bind worker API credentials to node identity before multi-worker prod exposure.
- [x] Require a bootstrap admin code for the first user on wiped catalogs.
- [x] Add a declarative one-time cutover wipe marker for prod hosts.
- [x] Update real-fleet tests to authenticate admin requests and clean up mutating workspaces.
- [x] Re-run prod mutating real-fleet tests on both worker nodes after widening cold-start readiness and fixing guest SSH firewalling.

## Prod Validation Notes

- First prod mutating run reached SSH in the backup-drill VM about two seconds after the 120s fresh-start deadline; this was build/copy latency for the generated microVM store disk, not a guest boot failure.
- Second prod mutating run proved the guest reached `sshd`, but host TCP/22 was dropped while ICMP worked. Use NixOS `allowedTCPPorts = [ 22 ]` instead of hand-written guest firewall rules.
- Third prod mutating run passed on `mom-1` and `mom-2`. Intentional VM stops still left failed systemd units because the runner exits 143 on SIGTERM; mark that as a successful exit status in the unit template.

## Fast Starts / Resumes

- [x] Keep production `master` deployed and healthy before branching.
- [x] Start branch `microvm-fast-start` from current `master` in `worktrees/microvm-fast-start`.
- [ ] Map prototype evidence for warm slots, snapshot/restore, and EROFS prebuild costs.
- [x] Implement the lowest-risk first fast-start improvement off `master`.
- [x] Validate locally and on prod-like hosts without landing to `master`.
- [x] Run another focused subagent review before any deploy decision.
- [x] Apply third-review deploy fixes and rerun coherent all-role validation after rolling a new pin.
- [x] Apply fourth-review predeploy fixes: fresh cutover marker, rollback/read-boundary docs, and workspace-source guard tests.
- [x] Rebase fast-start on the master API shutdown fix and redeploy the final pinned branch to all prod roles.
- [x] Apply fifth-review deploy fixes: service-aware cutover wipe, job-aware worker shutdown, retried/idempotent job completion, kernel machine-state lock, and graceful microVM shutdown.
- [x] Apply sixth-review deploy fixes: fail-closed cutover stop jobs, restore payload canonicalization, one active job per workspace, recovery supersedes stale jobs, and per-workspace worker lifecycle locks.
- [x] Run seventh predeploy review after merging fast-start to `master`.
- [x] Fix seventh-review control-plane findings: full-node lifecycle claim deadlock, worker-event freshness, stale-node CLI backup/restore queueing, concurrent create reservation, and lock-file clippy cleanup.
- [x] Repin prod configs to `master` with a fresh destructive `cutoverWipeMarker`; reusing `microvm-fast-start-cutover-v1` is a no-op because prod already created those marker files.
- [x] Run eighth predeploy review after master cutover.
- [x] Fix eighth-review findings: stale-node CLI lifecycle enqueueing, recovery target batch capacity, post-cutover marker removal, stricter failed-job monitor threshold, and dedicated Agent Mom switch recipe.
- [x] Run ninth predeploy review before the next prod deploy.
- [x] Fix ninth-review findings: require Hermes in generated guest flakes, prove SSH before reconcile marks VMs running, retry incomplete stopped/removed desired states, recover interrupted restore swaps, enforce CPU/memory placement and claim projections, declare service tunnel port/firewall wiring, align UI wait timeouts with cold-start limits, and include root-cause errors in structured worker logs.

## Fast Start Notes

- Prod cold starts are dominated by per-workspace Nix build and `microvm-store-disk.erofs`, not by the guest reaching multi-user after the runner starts.
- Sharing host `/nix/store` into the guest with read-only virtiofs removes `microvm-store-disk.erofs`; first measured host exec dropped to 21.8s, and same-VM restart dropped to 13.1s.
- Removing the scripted-initrd override was slower on `mom-1` with the store share: 28.1s first start and 16.6s restart. Keep scripted initrd for now.
- Next low-risk branch change: cache successful runner builds so stopped workspace restarts can skip no-op Nix evaluation and rebuild only when generated inputs change.
- Deployed the branch to `mom-1` only via configs branch `agentmom-fast-start-test`; branch runner cache preserved first start at 19.8s and reduced same-VM restart to 6.2s.
- Real API mutating tests against branch `mom-1` passed in 88.9s. Logs show first starts still build 26 derivations, while restore/restart starts skip Nix and reach SSH in about 5-6s.
- Deployed the same branch to `mom-2`; full real-fleet and mutating prod tests passed through the controller tunnel on both workers (`mom-1`: 98.5s, `mom-2`: 108.1s). Post-test sweeps showed ready API, active worker services, no failed units, and no leftover microVM units.
- Branch logs on both workers show the expected behavior: first starts build the generated runner closure, restarts skip Nix work, no `microvm-store-disk.erofs` is produced, and SSH is ready about 5-6s after service start.
- The second focused review found that existing stopped VM dirs could preserve old generated `microvm-workspace.nix` inputs and that mtime-based runner stamps could miss missing files or `flake.lock` changes. The branch now refreshes generated VM inputs before starting stopped VMs and caches runners by content hash in `.runner-input-hash`.
- Host `/nix/store` sharing is intentionally read-only but expands guest read access to host store contents. Before merge/deploy, validate from a guest that `/nix/store` and `/nix/.ro-store` are read-only and document that host store contents must not be treated as confidential from workspace guests.
- Earlier branch validation used new workers with the old controller. The next deploy-gate run should pin the fixed branch in configs and switch `mom-ctrl`, `mom-1`, and `mom-2` to the same Agent Mom package before rerunning real-fleet tests.
- Fixed branch commit `07a93f2` was pinned by configs test commit `61acc8d` and switched on `mom-ctrl`, `mom-1`, and `mom-2`. All three roles now evaluate/run the same Agent Mom package path.
- Coherent all-role validation passed: read-only real-fleet passed on both nodes, mutating real-fleet passed on `mom-1` in 99.1s and `mom-2` in 101.9s, and post-test sweeps showed ready API, active worker services, no failed units, and no leftover microVM units.
- Targeted guest validation from a disposable `storecheck-*` workspace confirmed `/nix/store` is mounted read-only from `ro-store` and `touch /nix/store/.agentmom-write-test` fails with `Read-only file system`.
- Botched `storecheck-*` attempts briefly tripped the monitor's recent-failed-job threshold; after the 900s window aged out, `agentmom-monitor-check` returned to `monitor ok` with no failed units.
- Third focused review found one deploy blocker: generated VM refresh rewrote `spec.json` in place. The branch now uses atomic same-directory writes for generated inputs, preflights workspace source paths before refresh, and adds systemd/journal diagnostics for start failures.
- Fixed commit `1d5dd0d` was pinned by configs test commit `c5737a6` and switched on `mom-ctrl`, `mom-1`, and `mom-2`. Local validation passed, all three NixOS toplevels built, read-only real-fleet passed on both workers, mutating real-fleet passed on `mom-1` in 95.39s and `mom-2` in 100.92s, and final sweeps showed ready API, monitor OK, active worker services, no failed units, and no leftover microVM units.
- Fourth focused review found no Rust or Nix packaging blockers. Ops review caught that prod still used the already-consumed `microvm-cutover-v2` marker, so the next destructive deploy would not wipe state. Configs now use `microvm-fast-start-cutover-v1`, and README documents the one-shot marker behavior, rollback cleanup for durable generated machine definitions, and the guest-readable host Nix store boundary.
- Destructive fast-start cutover deploy used configs commit `7cbc628` and Agent Mom `b982c29`. It switched `mom-ctrl`, `mom-1`, and `mom-2`, created `.microvm-fast-start-cutover-v1` markers, passed read-only real-fleet on both workers, and passed mutating real-fleet (`mom-1`: 96.69s, `mom-2`: 104.07s). Final sweeps showed monitor OK, no failed units, no leftover microVM units, and only the expected `host-check` workspace directories.
- Deploy logs exposed an existing API shutdown bug: `agentmom-api.service` waited for systemd timeout and got SIGKILL because long-lived worker SSE streams kept Axum graceful shutdown open. The fix `9e62e0c` landed on `master`; fast-start was rebased on it and force-pushed to `603b374`.
- Final configs commit `11f079b` pins Agent Mom `603b374`. It was switched to all prod roles and then `agentmom-api.service` was restarted live to prove the fixed process logs `api_shutdown` and deactivates successfully without timeout. Final read-only real-fleet passed on both workers, final mutating real-fleet passed (`mom-1`: 93.45s, `mom-2`: 100.23s), and final sweeps showed ready API, monitor OK, active worker services, no failed units, no post-01:01 errors, no leftover microVM units, and no machine dirs left behind.
- Fifth deploy review found four hardening gaps before the next deploy: marker-only cutovers did not explicitly stop old API/worker processes, worker restarts could kill active jobs, worker completion was a single non-idempotent POST, and the legacy mkdir state lock could survive crashes. The branch fixed those with service-aware cutover cleanup, longer job-aware worker shutdown, retried/idempotent completion, a kernel flock, and graceful microVM shutdown before forced kill; the next review then found more deploy blockers before this patch was rolled to prod.
- Sixth deploy review found additional predeploy blockers: cutover stop commands could replace queued API/worker starts, public restore jobs trusted caller-supplied backup locations, multiple same-workspace jobs could run concurrently, worker HTTP service opens could race destructive lifecycle jobs, and recovery restores could be blocked by stale active source-node jobs. Agent Mom commit `bab3126` fixes those by failing cutover stops closed with `--job-mode=fail`, canonicalizing restore jobs from catalog backups, validating backup IDs before restore temp paths, serializing same-workspace claims, holding worker-local per-workspace locks around lifecycle/service-open work, and terminalizing old workspace jobs during host-loss recovery. Configs commit `5aa1e9f` was switched to `mom-ctrl`, `mom-1`, and `mom-2`; read-only real-fleet passed on both workers, mutating real-fleet passed (`mom-1`: 96.81s, `mom-2`: 102.17s), and final sweeps showed ready API, monitor OK, active worker services, no failed units, no leftover microVM units, and only the expected `host-check` machine dirs.
- Seventh predeploy review confirmed the Nix/systemd shape has no additional blocker but caught that the prod configs still reuse the already-consumed `microvm-fast-start-cutover-v1` marker. It also caught control-plane gaps before merge-to-master deploy: workers under capacity pressure could not claim stop/remove jobs, worker job heartbeats did not refresh node freshness, remote CLI backup/restore could queue to stale ready nodes, and concurrent creates could over-reserve the last node slot. The code now gates only capacity-sensitive claims during pressure, touches node freshness on worker report paths without changing node status, requires fresh claimable nodes for remote CLI backup/restore, and reserves public/owned workspace placement inside an immediate SQLite transaction.
- Master cutover deployed Agent Mom `a00ac5c` with configs commit `1a886e3` and fresh marker `microvm-master-cutover-v1`. Toplevels switched to `mom-ctrl` `/nix/store/gqk4ls2mhy03b788csgmk3kfrk1dwkrg-nixos-system-mom-ctrl-26.05.20260522.a0991c8`, `mom-1` `/nix/store/w1fkg4g1iv1ynd4y8s9hhjd53rg7fp0j-nixos-system-mom-1-26.05.20260522.a0991c8`, and `mom-2` `/nix/store/1ml1wawfrzi6vwjixqbpkszqsnij51mp-nixos-system-mom-2-26.05.20260522.a0991c8`. Fresh cutover markers and archives exist on all three hosts. Local validation passed (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `nix flake check --no-build`); all three NixOS toplevels built; read-only real-fleet passed on both workers; mutating real-fleet passed (`mom-1`: 99.28s, `mom-2`: 121.86s). Final sweeps showed API ready, 2 ready nodes, 0 stale nodes, 0 queued jobs, 24 succeeded jobs, monitor OK, no failed units, active worker/bridge/proxy services, no leftover microVM units, no leftover machine dirs, and catalog backups saving restic snapshots after the cutover.
- Eighth predeploy review found no Nix/systemd blocker but surfaced deploy-process cleanup after the destructive cutover and two control-plane residual risks. Remote CLI start/stop/remove now require the assigned node to be fresh, claimable, and backed by a worker URL before queueing; failed stale remote starts do not change desired state. Host-loss recovery now rechecks target freshness, worker URL, ready status, and whole-batch active capacity inside an immediate transaction. Prod configs remove the one-shot cutover marker after it was consumed, set recent failed jobs to zero for monitor checks, and add `just agentmom-switch` for fail-closed role switches.
- Final hardening deploy used Agent Mom `592dbc3` and configs commit `c08c083`. Built toplevels were `mom-ctrl` `/nix/store/ha1bba2p131i7fxhaazw81frrcjxga63-nixos-system-mom-ctrl-26.05.20260522.a0991c8`, `mom-1` `/nix/store/ylqr7znn0da6x2vh2w39n9rcqkivfqsr-nixos-system-mom-1-26.05.20260522.a0991c8`, and `mom-2` `/nix/store/z3y3llf535bkvp7n2g9aw23iwaky55j2-nixos-system-mom-2-26.05.20260522.a0991c8`. Local validation passed (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `nix flake check --no-build`), focused review follow-up found no blockers, NixOS role eval/build passed, and `just agentmom-switch` switched all three roles. Read-only real-fleet passed on both workers; mutating real-fleet passed (`mom-1`: 99.89s, `mom-2`: 103.94s). Final sweeps showed 2 ready nodes, 0 stale nodes, 0 queued jobs, 48 succeeded jobs, strict monitor OK with 0 recent failed jobs, no failed units, active API/worker/bridge/proxy services, no leftover microVM units or machine dirs, no `agentmom-cutover-wipe.service`, and catalog backups saving restic snapshots after deploy.
- Ninth review confirmed the next deploy gate must include mutating real-fleet tests, because read-only health checks do not prove guest boot/SSH/Hermes. The code now pins Hermes as a generated workspace flake input instead of depending on ambient nixpkgs attrs, and all-systems `nix flake check --no-build` evaluates the Hermes package derivations.
