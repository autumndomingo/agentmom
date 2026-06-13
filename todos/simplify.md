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

## Plan Of Attack

Stabilize the test guardrails first. Fake fleet tests should be reliable under
the normal test command or explicitly serialized by the harness.

Rename the primary UI/API surface from VM to workspace. Add workspace-named
routes, move React calls off `/vms`, stop returning `vms` in new response
bodies, and keep compatibility aliases only as long as they are useful.

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

Hide legacy direct sandbox commands. The supported operator shape is workspace,
node, fleet, db, config, api, and worker. Raw sandbox lifecycle commands belong
behind an internal/debug namespace if they remain at all.

Tighten config around the happy path. Generated config and Nix examples should
prefer `openrouter-proxy`; guest auth file mode should stay explicit and
experimental.

Keep runners dumb. The API owns placement and recovery. Workers claim assigned
jobs, operate local VMs and volumes, open local services, run backup/restore,
and report events and artifacts.

Keep image preparation boring. Deploy preflight should validate config, build or
verify the configured snapshot, and fail before replacing the worker if the
image cannot boot and pass probes.

Keep backup and recovery boring. Restic is the only workspace backup path for
now, and lost-host recovery remains an explicit operator command.
