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
- Prod hosts use `cutoverWipeMarker = "microvm-cutover-v2"` to move old catalog/runtime state aside once under systemd before startup.

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
- [x] Apply review fixes and rerun coherent all-role validation.

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
