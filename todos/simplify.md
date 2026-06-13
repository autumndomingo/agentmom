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
- Hide OpenCode and Codex subscription flows from the primary UI.
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
- `/w/<workspace-slug>/opencode` only when explicitly enabled

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
- Kept OpenCode authorization before the feature flag check. A caller must be
  allowed to access the workspace before learning whether OpenCode is enabled.
- Kept OpenCode hidden by default through the single Agent Mom config system:
  `features.opencode = true` enables the API route and the browser button. Do
  not add side-channel env vars such as `MOM_ENABLE_OPENCODE` or Vite build
  flags for product behavior.

Temporary route decision:

The React workspace list currently uses the auth-filtered legacy compatibility
route `/api/vms`, then normalizes the result as workspaces. This is intentional
after the auth rebase. The canonical core `GET /api/workspaces` still belongs
to the scheduler/control-plane API and currently returns all workspaces, while
the auth-filtered browser list is implemented in `src/ui.rs` behind `/api/vms`.
Moving the browser list back to `/api/workspaces` should be paired with one of
these explicit follow-up decisions:

- make core `GET /api/workspaces` cookie-aware and filtered for browser users,
  while preserving an operator/admin path for full fleet listing;
- split the unauthenticated/internal scheduler API away from public browser
  `/api/workspaces`; or
- add a new workspace-named browser list route that cannot be shadowed by the
  existing core route.

Until that API boundary is made explicit, keeping the one `/api/vms` list call
is the safer inconsistency. Workspace action routes already use
`/api/workspaces/<name>/...` and are auth-checked.

## Plan Of Attack

Stabilize the test guardrails first. Fake fleet tests should be reliable under
the normal test command or explicitly serialized by the harness.

Rename the primary UI/API surface from VM to workspace. Add workspace-named
routes and stop returning `vms` in new response bodies. The browser action
routes should use `/api/workspaces/...`; the browser list route remains the
auth-filtered `/api/vms` compatibility endpoint until the canonical
`/api/workspaces` list is secured or split from the internal control-plane API.

Make Hermes the only default visible service. OpenCode should require an
explicit enable flag. Codex prompt flows should not appear in the primary UI.

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
missing capability such as permissions, richer tool timeline, durable session
control, or a need for OpenCode parity.

Replace node-port service URLs with stable workspace routes. The API should
resolve workspace to assigned node and service internally; tunnel hostnames
should be implementation detail.

HARD CUT legacy direct sandbox commands. The supported operator shape is
workspace, node, fleet, db, config, api, and worker. Do not hide old top-level
commands behind compatibility aliases; remove them from the CLI. There are no
external users to preserve, and deployments/databases can be reset while this is
still early. Raw sandbox helpers may remain as Rust internals only when
workspace/node code needs them. After this pass, the unused raw lifecycle
wrappers were removed too; workspace-scoped exec/codex/hermes and base snapshot
doctor paths remain because they are still part of the supported flow.

Tighten config around the happy path. Generated config and Nix examples should
prefer `openrouter-proxy`; guest auth file mode should stay explicit and
experimental.

All product/runtime flags should flow through the Agent Mom config file or the
Nix module that generates it. Environment variables are still fine for process
placement/secrets that are already process-local (`MOM_CONFIG`,
`MOM_STATE_DIR`, worker URL/token plumbing, test-only fake runtime), but not for
feature switches such as OpenCode visibility.

Keep runners dumb. The API owns placement and recovery. Workers claim assigned
jobs, operate local VMs and volumes, open local services, run backup/restore,
and report events and artifacts.

Keep image preparation boring. Deploy preflight should validate config, build or
verify the configured snapshot, and fail before replacing the worker if the
image cannot boot and pass probes.

Keep backup and recovery boring. Restic is the only workspace backup path for
now, and lost-host recovery remains an explicit operator command.
