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

Build the first Mom-native chat widget against Hermes ACP, not the dashboard
TUI. The browser talks only to `mom api`; the API enforces workspace ownership
and proxies a WebSocket to the assigned worker; the worker supervises
`hermes-acp` inside the workspace and pipes raw JSON-RPC frames between browser
and ACP stdio.

Correction after review: the first REST-shaped ACP adapter was the wrong mental
model. Agent Mom should not translate ACP into REST, advertise client-side file
capabilities, or make Rust understand Hermes chat semantics. Hermes runs inside
the workspace and owns its tools/filesystem behavior. Mom is the auth/routing
and process shell. The implemented direction is one dumb durable pipe:
`/api/workspaces/<workspace>/chat/ws` to `/worker/hermes-acp/ws` to
`hermes-acp` stdio. The UI sends `initialize`, `session/new`, `session/prompt`,
cancel notifications, and permission responses as raw JSON-RPC messages.

Keep the full Hermes dashboard as an advanced/debug escape hatch. Do not make
the terminal-rendered TUI the primary chat experience.

Use the stale `chat-acp` worktree as research, not as a merge target. Its live
QA proves Hermes ACP can carry the experience we want, including streaming
assistant text, resource attachments, thinking/tool updates, permissions, and
session controls. The grug version is Hermes-only: no OpenCode agent enum, no
generic multi-harness selector, no `/api/vms` noun revival, and no old UI auth
state. Port the useful pieces: ACP process supervision, JSON-RPC event capture,
permission response handling, transcript normalization, and chat rendering. Drop
the generic fallback surface unless a Hermes use case needs it. The actual port
deliberately skipped assistant-ui for now; the built-in Mom message renderer was
enough for streamed text, thinking/tool/status cards, raw fallbacks, and
permissions. Add assistant-ui only when the local renderer becomes real
complexity.

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

## Implementation Notes

- Replaced the initial REST-shaped ACP bridge with a raw WebSocket pipe.
  `src/acp.rs` now launches `hermes-acp` over a microsandbox SSH stdio bridge
  and pipes WebSocket text frames to/from line-delimited ACP JSON-RPC.
- Added cookie-authenticated browser route
  `/api/workspaces/<workspace>/chat/ws`, which proxies to bearer-token worker
  route `/worker/hermes-acp/ws`.
- The worker validates workspace/node/sandbox assignment before upgrading the
  WebSocket. Fake runtime returns a fake `mom/status` frame and echoes JSON-RPC
  so tests do not need real Hermes.
- Base snapshot provisioning now installs `hermes-agent[all,messaging,acp]`;
  this is the deployment guardrail that keeps the adapter binary present.
- Deleted Rust filesystem callbacks and FS capability advertising. If Hermes
  sends an unadvertised client request later, surface it explicitly and decide
  whether it belongs in Mom rather than carrying generic ACP baggage.
- Verified with `cargo check`, `cargo test`, `npm --prefix ui run build`,
  `git diff --check`, and a one-off `npx agent-browser` smoke against Vite on
  `127.0.0.1:5177`.
- Added fake-fleet WebSocket coverage for `/api/workspaces/<workspace>/chat/ws`
  routing through the assigned worker and carrying raw JSON-RPC frames.
- Real `just dev` exposed a non-ACP runtime bug: the worktree-local
  `.state/msb` path made Microsandbox's derived relay socket exceed macOS Unix
  socket limits, so the worker could not create the sandbox and Hermes never
  launched. Dev now uses a short per-repo `/tmp/mom-msb-*` home for
  Microsandbox, and the browser WebSocket gets a concrete `mom/status` error
  when the API cannot reach the worker ACP socket.
- Browser e2e then reached Hermes itself. `initialize` succeeded, but
  `session/new` exposed two launch/protocol hygiene issues: the ACP process must
  be started with the configured `HERMES_HOME`, and Hermes 0.16 can print
  dependency setup chatter to stdout before its JSON-RPC response. Mom still
  stays dumb: it does not interpret ACP, but it now keeps the stdout transport
  JSON-RPC-only by dropping non-JSON child output and logging it in the worker.
- Playwright caught a UI readiness race that fake tests missed: the transport
  status frame said `ready`, so the composer enabled before ACP `session/new`
  returned and sent `session/prompt` with `sessionId: null`. Transport status is
  now `connected`; only the JSON-RPC `session/new` result can make chat `ready`.
- Added Playwright to the Nix dev shell because browser-level WebSocket frame
  capture is now part of this feature's expected verification. The shell exposes
  `playwright`, `require("playwright-core")`, and Nix-managed browsers without
  ad hoc npm browser downloads.
- The next Playwright pass showed browser ACP still lacked provider auth even
  with `HERMES_HOME`; the live `initialize` auth methods omitted OpenRouter.
  Direct workspace exec had sourced the guest proxy profile, while the ACP SSH
  command had not. The ACP launch now sources `/etc/profile.d/agentmom-proxy.sh`
  before `exec hermes-acp`, keeping provider/proxy configuration in the one
  generated guest config path.
- Dev credentials now use ignored repo-root `.env`, loaded by `just`. `just dev`
  writes the local OpenRouter key into `.state/iron-proxy/openrouter-api-key`,
  generates `.state/iron-proxy/config.yaml`, starts iron-proxy on
  host loopback port `1080`, unsets the raw env var before API/worker launch,
  and stops the proxy with the API/worker. Hard cut after review: no direct-key
  ACP fallback, no dev-only Hermes credential mode, and no second Hermes config.
  Hermes ACP always sources the generated guest proxy profile and uses the same
  `credentials.proxy_url` path as production. The missing piece was sandbox
  networking/config: `127.0.0.1:1080` was guest loopback, not the host proxy,
  and Microsandbox's default policy allowed public egress but only host DNS.
  `config.dev.json` now uses `host.microsandbox.internal:1080`, and
  workspace/base sandboxes install an explicit policy for public egress, host
  DNS, and host TCP `1080`. If the key is missing, `just dev` fails before
  startup.
- The live OpenRouter model catalog showed `openai/gpt-4o-mini` is available,
  so `config.dev.json` now pins that model. This keeps dev e2e focused on Mom
  and ACP behavior instead of a stale default model id.
- Subagent review confirmed the hard-cut direction: delete the direct-key ACP
  fallback rather than preserving dev/prod credential drift. Pulled only the
  useful frontend pieces from `simplify-acp`: RPC response method metadata,
  chunk-aware transcript normalization, permission/tool/content block rendering,
  and matching block styles. Skipped its Rust ACP code because it regressed
  readiness, proxy sourcing, and non-JSON stdout filtering.
- Final local e2e after the hard cut: reset dev DB/workspaces, onboarded a
  fresh admin/workspace, verified `mom workspace proxy-smoke` from inside the
  VM, then Playwright logged in and sent a Hermes ACP prompt through
  `/api/workspaces/.../chat/ws`. Hermes returned the unique token through
  OpenRouter via iron-proxy, with no ACP control bookkeeping rendered in chat.
- Prompt lifecycle tightened: the composer remains busy after `session/prompt`
  until the matching JSON-RPC response, websocket error, close, or local send
  failure. This prevents overlapping turns while Hermes is still streaming.
- Added `just dev-reset` as a dev-only runtime broom. The primary reset
  contract is pid-file based: `just dev` writes `.state/dev.pid` for the dev
  runner, iron-proxy, API, and worker. A later live failure showed pid files are
  not enough for stale Microsandbox VM processes that were launched before the
  pid-file discipline or survived parent shutdown, so reset also kills only
  processes whose command contains this worktree's repo-scoped
  `/tmp/mom-msb-*` home. Still no broad port scanning, no DB surgery, and no
  matching unrelated `mom`/`msb` processes.
- Sidebar chat state is now Hermes session state instead of a disguised
  workspace list. The backend ACP path remains a raw pipe. The UI keeps one ACP
  websocket per selected workspace, calls Hermes `session/list` after
  `initialize`, renders those Hermes sessions in the sidebar, calls
  `session/load` on startup/selection, and uses Hermes session IDs as chat IDs.
  Temporary local IDs exist only before `session/new` or `session/fork` returns.
  The grug rule is: browser product state may organize ACP frames, but Rust
  does not learn Hermes chat semantics.
- Concurrent `just dev` in one worktree did clobber `.state/dev.env` and could
  leave the UI/API/worker disagreeing about dynamic ports. `scripts/dev-run`
  now takes a worktree-local `.state/dev.lock` before writing env/state and
  fails fast with a `just dev-reset` hint if another live dev runner owns it.
  Stale locks are removed only when the recorded owner pid is dead.
- Real Hermes ACP over Microsandbox SSH requires a TTY in this environment; the
  no-PTY stdio path fails before Hermes can speak JSON-RPC. The bridge now uses
  `ssh -tt` and disables terminal echo in the guest. Because PTY echo can still
  race, the bridge also drops exact echoes of frames it just wrote before
  forwarding stdout to the browser. This stays transport-level only: Mom still
  does not parse or translate ACP.
- Hermes ACP then blocked on an interactive browser-engine install prompt. That
  is not acceptable for live chat because the websocket looks connected while
  Hermes waits for input. Base provisioning now installs `nodejs`, `npm`,
  Alpine `chromium`, and `agent-browser`, then writes root's
  `~/.agent-browser/config.json` to use the system Chromium binary. The
  Chrome-for-Testing downloader path is intentionally not used because it has no
  Linux ARM64 build.
- ACP readiness was split into explicit UI states after live debugging. The
  raw backend pipe worked when driven as `initialize -> session/new ->
  session/prompt`, but the React app treated socket-open as ACP-initialized and
  fired `session/new` before the `initialize` response. The UI now uses
  `initializing` for the open socket, treats `mom/status connected` as
  transport information only, and only the `initialize` result moves the
  workspace to `open` where chat session creation is allowed. A Playwright E2E
  then verified two chats/two Hermes sessions and a real OpenRouter-backed
  prompt rendering in the transcript.
- The Hermes chat UI now covers the ACP must-haves reviewed on 2026-06-13:
  `session/list`, `session/load` with `session/resume` fallback, `session/new`,
  `session/prompt`, `session/cancel`, `session/fork`, `session/close`,
  `session/set_model`, `session/set_mode`, `session/set_config_option`,
  permission responses, and image prompt blocks. Model/mode/config controls are
  rendered only from Hermes responses. The UI does not synthesize an alternate
  Mom chat API.
- A real browser race showed `session/list` was computing the next active chat
  inside an async React updater and then immediately calling `session/load` with
  potentially stale state. Chat/session mutations now update refs and React
  state together before dependent ACP calls. New chat is disabled until ACP is
  fully `ready`, so users cannot create a temporary local chat while startup
  `session/load` is still in flight.
- Fork needed one more state-machine guard: creating a temporary fork chat made
  the generic "active chat has no session" effect send a stray `session/new`
  alongside `session/fork`. Fork temps are now marked `creatingSession` before
  selection, so the only ACP method sent by the Fork button is `session/fork`.
- Playwright e2e now exercises the real dev stack through cookies and
  `/api/workspaces/<workspace>/chat/ws`: login, `initialize`, `session/list`,
  `session/load`, `session/new`, OpenRouter-backed `session/prompt`, wait for
  the prompt response and post-prompt `session/list`, reload, and verify the
  token is restored from Hermes history. A targeted fork e2e verified
  `session/fork` responds without any stray `session/new`. A targeted close e2e
  created an empty Hermes session, clicked the sidebar close control, observed
  `session/close`, and refreshed `session/list`.
