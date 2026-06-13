Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# Simplify Agent Mom

## Intent

Make Agent Mom boring enough to operate and change without fear.

After the auth work lands, the foundation is `mom api` as the central catalog,
browser-auth host, UI host, scheduler, and public control plane. Workers are
private executors. Workspaces are durable; VMs are disposable runtime wrappers
around workspace volumes.

The production story should be easy to say:

- A user signs in through `mom api`.
- The API owns the user's workspace record and placement.
- A runner claims assigned jobs and reports facts.
- Hermes runs in the workspace through OpenRouter via iron-proxy.
- Backup and recovery use restic and explicit operator commands.

## Philosophy

Grug-brain rules for this pass:

- Prefer deleting or hiding modes over abstracting them.
- Keep one obvious happy path before polishing edge paths.
- Make small changes that leave the system working after each step.
- Understand why a thing exists before removing it.
- Accept simple duplication when shared machinery would be harder to reason
  about.
- Use fake fleet behavior tests as the main guardrail.
- Log important state changes so distributed behavior is debuggable.

## Scope

In scope:

- Rename public UI/API concepts from VM to workspace.
- Make Hermes/OpenRouter/proxy the default production path.
- Delete OpenCode and Codex subscription auth flows from the baseline.
- Add a Mom-native chat widget for the Hermes production path.
- Hide or remove top-level direct sandbox commands.
- Replace node-port service URLs with stable workspace routes.
- Keep worker behavior limited to assigned local execution and reporting.
- Keep image preparation, backup, and recovery explicit.

Out of scope:

- Kubernetes/k3s.
- Full HA or automatic failover.
- Codex subscription token brokerage.
- Burst VPS autoscaling.
- Replacing SQLite.
- Rebuilding the frontend design from scratch.
- OpenCode service parity.

## Target Shape

Operator CLI:

- `mom workspace ...`
- `mom node ...`
- `mom fleet ...`
- `mom db ...`
- `mom config ...`
- `mom api`
- `mom worker`

Public browser routes:

- `/`
- `/w/<workspace-slug>/hermes`

Public API routes:

- `/api/workspaces`
- `/api/workspaces/<workspace-slug>/...`
- `/api/workspaces/<workspace-slug>/chat/...`
- `/api/me`
- `/api/admin/...`

Private worker routes:

- `/worker/...`

## Notes From 2026-06-13 Auth Rebase

Auth landed in `origin/master` as a material architecture change, not just a UI
login patch. `mom api` now owns browser sessions, admin invite/access-code
management, user setup, and user-owned workspace creation. The old standalone
`mom-ui` binary is gone; the React app is served by `mom api` and calls
same-origin `/api` routes with first-party cookies. That means frontend state
must not reintroduce localStorage session tokens, bearer tokens, or role strings
from the old UI. The live role strings are now `admin` and `user`.

Important auth boundaries observed during the rebase:

- `/api/auth/login`, `/api/me`, `/api/me/setup`, and `/api/admin/...` live in
  `src/auth.rs`.
- `/api/me/setup` updates the user's name, creates the owned workspace if one
  does not exist, queues the create job, and returns the refreshed session. The
  browser should use this instead of directly posting workspace creates during
  onboarding.
- `src/ui.rs` browser/workspace action routes must call `authorize_workspace`
  or `require_admin`. During conflict resolution, keep those auth checks before
  service opens and job creation.
- Worker endpoints remain bearer-token authenticated. Do not mix browser cookie
  auth into `/worker/...`.

Conflict-resolution decisions made during the rebase:

- Kept auth branch cookie sessions and `/api/me/setup`; discarded the older
  client-side onboarding flow that created a workspace and then listed
  workspaces to rediscover it.
- Preserved admin cookies in fake fleet tests while changing route names.
  Tests that exercise browser-facing routes should include the admin session
  cookie unless they are explicitly testing unauthorized behavior.
- Resolved the `/api/vms` noun mismatch with the hard browser-auth choice:
  `GET /api/workspaces` is now cookie-aware and filtered via
  `visible_workspaces`, `POST /api/workspaces` requires admin, workspace events
  require workspace authorization, and `/api/jobs` create/get require a session
  authorized for the job workspace. Worker polling stays on bearer-token
  `/worker/workspaces`.
- HARD CUT OpenCode and OpenAI subscription auth. OpenCode service config,
  routes, UI controls, tests, and Nix options are gone. `vm-auth-json`,
  copied Codex/OpenCode auth paths, and legacy flat config migration are gone.
  The baseline is Hermes with OpenRouter proxy credentials. This is intentional:
  history can recover the deleted paths if they become necessary, but the
  product surface should not carry them as dormant complexity.
- Dev config now follows the same config path as production. `config.dev.json`
  points at proxy credentials, and `scripts/dev-env` creates ignored local CA
  material so `mom config doctor` is valid before the real dev proxy is started.
  The dev proxy service itself is still an operator/dev prerequisite for real
  Hermes model calls; we are not adding a second config mode just for local
  convenience.
- Follow-up cleanup removed generic service matching that only had one surviving
  variant. Worker service open is now the explicit Hermes endpoint, fake runtime
  opens Hermes directly, and the tunnel code uses Hermes constants instead of a
  reusable service enum/spec shape. This keeps the hard cut reflected in the
  code, not just hidden behind one-option abstractions.

## Plan Of Attack

Stabilize the test guardrails first. Fake fleet tests should be reliable under
the normal test command or explicitly serialized by the harness.

Rename the primary UI/API surface from VM to workspace. Add workspace-named
routes and stop returning `vms` in new response bodies. The browser now uses
cookie-authenticated `/api/workspaces`; worker/internal listing remains under
`/worker/workspaces`.

Make Hermes the only visible service. OpenCode and Codex prompt flows are not
baseline features; add them back from history only with a concrete product need.

Build the first Mom-native chat widget against the Hermes API server. Keep it
small: session list, message history, composer, streaming response, cancel if
Hermes exposes it cleanly, and clear errors. The browser talks only to `mom api`;
the API enforces workspace ownership and proxies to the assigned worker, and the
worker talks to Hermes inside the workspace.

Keep the full Hermes dashboard as an advanced/debug escape hatch. Do not make
the terminal-rendered TUI the primary chat experience.

Defer ACP. The chat worktree prototype proves ACP can work, but generic ACP adds
process supervision, JSON-RPC event translation, permission plumbing, and
multi-agent complexity. Revisit ACP only after the Hermes API chat hits a real
missing capability such as permissions, richer tool timeline, or durable
session control.

Replace node-port service URLs with stable workspace routes. The API should
resolve workspace to assigned node and service internally; tunnel hostnames
should be implementation detail.

HARD CUT legacy direct sandbox commands. The supported operator shape is
workspace, node, fleet, db, config, api, and worker. Do not hide old top-level
commands behind compatibility aliases; remove them from the CLI. There are no
external users to preserve, and deployments/databases can be reset while this is
still early. Raw sandbox helpers may remain as Rust internals only when
workspace/node code needs them. After this pass, the unused raw lifecycle
wrappers were removed too; workspace-scoped exec/hermes and base snapshot doctor
paths remain because they are still part of the supported flow.

Tighten config around the happy path. Generated config and Nix examples should
use OpenRouter proxy credentials directly; guest auth file mode is removed.

All product/runtime flags should flow through the Agent Mom config file or the
Nix module that generates it. Environment variables are still fine for process
placement/secrets that are already process-local (`MOM_CONFIG`,
`MOM_STATE_DIR`, worker URL/token plumbing, test-only fake runtime), but not for
product behavior.

Keep runners dumb. The API owns placement and recovery. Workers claim assigned
jobs, operate local VMs and volumes, open local services, run backup/restore,
and report events and artifacts.

Keep image preparation boring. Deploy preflight should validate config, build or
verify the configured snapshot, and fail before replacing the worker if the
image cannot boot and pass probes.

Keep backup and recovery boring. Restic is the only workspace backup path for
now, and lost-host recovery remains an explicit operator command.
