# Remote Agent Mom Dev Notes

Notes from testing the admin VM version/actions work on mom-2.

## What worked

- A dirty local worktree can be rsynced to `mom-2:~/code/agentmom-dev/<name>` and tested with `nix develop`.
- After the first cold build, remote checks are fast enough for normal iteration.
- Fake-runtime fleet tests are a good safe smoke test for admin control-plane changes:
  - `npm --prefix ui run build`
  - `cargo test --test fleet_fake admin_infra_overview_returns_fleet_snapshot -- --test-threads=1`
  - `cargo test --test fleet_fake fake_worker_start_stop_backup_jobs_update_central_state -- --test-threads=1`

## Friction

- `v --remote mom-2` is not the right default for Agent Mom host-runtime testing.
  It starts an extra worktree VM, assumes a remote git checkout path, and does not sync dirty local edits.
  Agent Mom microVM testing wants direct access to the mom-2 host runtime, not nested virtualization.
- The current branch did not have `scripts/dev-remote`, even though an older mom-2 copy did.
  That makes remote dev knowledge too easy to lose.
- Direct `just dev` on mom-2 is not yet a safe full-runtime dev bed because the deployed
  `agentmom-microvm@.service` template is still tied to `/data/agentmom` and the deployed package.
- `just dev` has no fake-runtime mode. For UI/control-plane checks, we currently have to start
  `mom api` and `mom worker` manually with `MOM_RUNTIME=fake`.
- First remote run spent several minutes in Nix/dev-shell setup and Cargo cold compile.
- One long SSH command dropped mid-compile. Re-running with
  `-o ControlMaster=no -o ServerAliveInterval=30 -o ServerAliveCountMax=6` worked.
- Do not assume `python3` exists on the remote NixOS host. `jq` was available.

## Improvements worth doing

- Bring a maintained `scripts/dev-remote` / `just dev-remote-*` workflow into the active branch.
  It should sync dirty worktrees, preserve remote `.env`, use keepalives, and print the exact URL.
- Add `just dev-fake` for UI/control-plane development. It should run API + worker with
  `MOM_RUNTIME=fake`, skip real microVM runtime checks, seed optional demo data, and serve the built UI.
- Add a dev-specific microVM systemd template or runner mode before advertising full `just dev`
  on mom-2. It must use per-checkout state paths and the checkout-built `mom`, not `/data/agentmom`.
- Make `v --remote` fail helpfully for Agent Mom if the remote repo path is missing, or document the
  correct override. It is useful for generic Linux worktree VMs, but it should not be presented as
  the main Agent Mom host-runtime test path.
- Prewarm mom-2 dev caches for this repo so the first remote test is not dominated by Nix/Cargo setup.
