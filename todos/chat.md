Living plan. Revise it as we learn. Do not treat this as a fixed contract.

# Agent Mom Chat

## Intent

Make Agent Mom a useful shell for agent workspaces without committing too early to one chat integration strategy. We want users to reach OpenCode, Hermes, and future harnesses from a selected VM, while keeping Agent Mom responsible for VM lifecycle, auth seeding, service launch, and routing.

## Scope

- Track three possible chat/app integration paths.
- Start by embedding a whole Hermes UI on its own page, matching the existing OpenCode full UI path.
- Keep the existing OpenCode full UI launcher and add a Hermes launcher beside it.
- Preserve room to later try ACP or assistant-ui runtimes without making either the default now.
- Keep VM lifecycle, auth seeding, and full harness app launchers owned by Agent Mom.

Out of scope for the first pass:

- Maintaining custom Agent Mom chat components for every harness.
- Restyling OpenCode/Hermes to look identical.
- Replacing full OpenCode/Hermes dashboards.
- Perfect multi-chat history or unified transcript storage.

## Approach

There are three viable paths. We may try all of them, but we should evaluate them as separate product surfaces instead of forcing one abstraction to solve everything.

### Path 1: Embed Agent-Specific UIs

Run the harness' own web UI and embed or open it from Agent Mom.

```text
Selected VM
  -> launch OpenCode web UI / Hermes web UI
  -> Agent Mom reverse proxy/tunnel
  -> embedded page or top-right launcher button
```

This is the least custom UI work and preserves full harness-specific behavior, including settings, permissions, crons, tools, sessions, logs, and other admin surfaces. Styling may differ between harnesses, and that is acceptable.

### Path 2: ACP-Based Unified Chat

Use ACP as a common protocol for a Mom-native chat view.

```text
Selected VM
  -> launch ACP agent process: opencode acp or hermes-acp
  -> Agent Mom ACP client/supervisor
  -> Mom chat API / websocket
  -> shared Mom chat UI
```

This gives a consistent UI and broad harness compatibility, but may flatten or lose harness-specific richness.

### Path 3: assistant-ui Runtime Per Harness

Use assistant-ui as the shell and write/select one runtime per harness.

```text
OpenCode -> @assistant-ui/react-opencode
Hermes   -> custom or future assistant-ui runtime
OpenClaw -> custom or future assistant-ui runtime
```

This may give the best embedded React chat experience where a runtime exists, but we should avoid building and maintaining separate full chat renderers ourselves.

## Steps

- [x] Audit current full UI launcher boundary.
  - Identify where workspace selection, OpenCode launch, and chat submit currently live.
  - Identify the service/tunnel state needed to add another harness UI.
  - Decide whether the first Hermes version opens externally, embeds inline, or supports both.

- [x] Add Hermes full UI page/launcher.
  - Add `Hermes` beside `Refresh` and `OpenCode`.
  - Start or discover the Hermes web/dashboard service for the selected VM.
  - Open it the same way OpenCode opens today.
  - Prefer a whole embedded UI over a custom chat component.

- [x] Decide which Hermes UI to run first.
  - Official `hermes dashboard` preserves Hermes-supported admin/config behavior.
  - Community Hermes WebUI may provide a more browser-native chat surface.
  - Record the selected default and keep the launch command configurable.

- [ ] Add an app/view model.
  - Track available apps per VM: `mom`, `opencode`, `hermes`.
  - Allow the main pane to switch between embedded app surfaces.
  - Keep the left navigation simple until we decide whether chats or apps should own it.

- [ ] Revisit Mom-native chat after embedded Hermes works.
  - Evaluate ACP as a generic fallback.
  - Evaluate `@assistant-ui/react-opencode` for OpenCode-specific embedded React chat.
  - Only build Hermes/OpenClaw assistant-ui runtimes if embedding full UIs proves insufficient.

- [ ] Verify embedded full UIs.
  - OpenCode launches for a selected VM as it does today.
  - Hermes launches for a selected VM.
  - Switching selected VMs does not cross-wire services or auth.
  - Embedded and external-open modes both work if both are supported.

## Implementation Notes

- OpenCode ACP is available as `opencode acp` and already bridges OpenCode HTTP/SSE into ACP events.
- Hermes ACP is available as `hermes-acp` / `hermes acp` and exposes chat, tool activity, diffs, terminal commands, approval prompts, and streamed thinking/response chunks.
- assistant-ui has an experimental OpenCode runtime, `@assistant-ui/react-opencode`.
- No equivalent assistant-ui Hermes or OpenClaw runtime is currently known.
- The current product direction is to try embedding whole harness UIs first, starting with Hermes, because that avoids maintaining custom chat renderers and preserves each tool's full model.
- First Hermes implementation uses the official `hermes dashboard` on guest port `9119`, exposed through the same SSH tunnel pattern as OpenCode. The first UI opens externally; inline embedding can be added after the launch path is stable.
- On Alpine, the official dashboard can lazy-install Discord messaging deps on startup. Snapshot provisioning now installs `hermes-agent[all,messaging]` with Clang/compiler-rt so `brotlicffi` builds before users launch the dashboard.
