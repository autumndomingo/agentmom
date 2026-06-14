Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# MicroVM Prod Hardening

## Intent

Make the hard-cut microvm.nix runtime deployable to production with old Agent Mom data wiped. Prefer fewer runtime knobs, stronger startup validation, and declarative Nix/systemd behavior.

## Scope

Do:
- Fix proxy, bridge, locked-input, runtime validation, and local/remote routing issues.
- Remove unsupported or unused configuration surface.
- Simplify workspace VM creation and fake-runtime wiring where it is low risk.

Do not:
- Preserve microsandbox compatibility.
- Preserve old fleet DB data or migrations.
- Implement warm pools or snapshot/restore in this pass.

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
