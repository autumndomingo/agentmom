import {
  AssistantRuntimeProvider,
  ThreadPrimitive,
  useExternalStoreRuntime,
  type AppendMessage,
  type ThreadMessageLike
} from "@assistant-ui/react";
import {
  ChevronDown,
  FilePen,
  FilePlus2,
  FileText,
  GitBranch,
  Lightbulb,
  LogOut,
  MessageCircle,
  PanelRightOpen,
  SquarePen,
  Terminal,
  UserPlus
} from "lucide-react";
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type {
  AppState,
  ChatMessage,
  MeState,
  MessageAttachment,
  PreviewService,
  SessionSummary,
  UiEvent
} from "../src/types.js";
import { AdminPage } from "./admin.js";
import { AttachmentComposer, MessageAttachments } from "./attachment-composer.js";
import { AuthScreen, LoadingScreen } from "./auth.js";
import { readError, readJsonResponse, readResponseError } from "./http.js";
import { RightPanel } from "./right-panel.js";
import { TelegramSettingsPage } from "./telegram-settings.js";
import "@fontsource-variable/dm-sans";
import "@fontsource-variable/fraunces";
import "./pages.css";
import "./styles.css";
import "./thread.css";

const emptyState: AppState = {
  app: {
    sourceDir: ""
  },
  workspace: "",
  projectsDir: "",
  agentCwd: "",
  sessionDir: "",
  sessions: [],
  previews: [],
  messages: [],
  events: [],
  isRunning: false,
  model: "",
  tools: [],
  runtime: {
    executor: "local"
  }
};

function App() {
  const [me, setMe] = useState<MeState | undefined>();
  const [authChecked, setAuthChecked] = useState(false);
  const [authEnabled, setAuthEnabled] = useState(false);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<string | undefined>();
  const [state, setState] = useState<AppState>(emptyState);
  const [error, setError] = useState<string | undefined>();
  const [rightPanelOpen, setRightPanelOpen] = useState(false);
  const isAdminPage = window.location.pathname === "/admin";
  const isTelegramSettingsPage = window.location.pathname === "/settings/telegram";

  const selectedWorkspace = useMemo(
    () => me?.workspaces.find((workspace) => workspace.id === selectedWorkspaceId) ?? me?.workspace,
    [me, selectedWorkspaceId]
  );

  const loadMe = useCallback(async () => {
    const response = await fetch("/api/me");
    const payload = await readJsonResponse(response);
    setAuthEnabled(Boolean(payload.authEnabled));
    if (response.ok) {
      const next = payload as MeState;
      setMe(next);
      setSelectedWorkspaceId((current) => current ?? next.workspace.id);
    } else {
      setMe(undefined);
    }
    setAuthChecked(true);
  }, []);

  const workspaceUrl = useCallback(
    (path: string) => {
      if (!selectedWorkspace?.id) throw new Error("workspace is not loaded");
      return `/api/workspaces/${encodeURIComponent(selectedWorkspace.id)}${path}`;
    },
    [selectedWorkspace?.id]
  );

  const refresh = useCallback(async () => {
    if (!selectedWorkspace?.id) return;
    const response = await fetch(workspaceUrl("/state"));
    if (!response.ok) throw new Error(await readResponseError(response));
    setState((await response.json()) as AppState);
    setError(undefined);
  }, [selectedWorkspace?.id, workspaceUrl]);

  useEffect(() => {
    void loadMe().catch((err) => {
      setError(readError(err));
      setAuthChecked(true);
    });
  }, [loadMe]);

  useEffect(() => {
    if (isAdminPage || isTelegramSettingsPage || !selectedWorkspace?.id) return;
    let cancelled = false;
    let events: EventSource | undefined;

    void refresh()
      .then(() => {
        if (cancelled) return;
        events = new EventSource(workspaceUrl("/events"));
        events.addEventListener("state", (event) => {
          setState(JSON.parse((event as MessageEvent).data) as AppState);
          setError(undefined);
        });
        events.onerror = () => setError("Event stream disconnected. Refresh or restart the dev server.");
      })
      .catch((err) => {
        if (!cancelled) setError(readError(err));
      });

    return () => {
      cancelled = true;
      events?.close();
    };
  }, [isAdminPage, isTelegramSettingsPage, refresh, selectedWorkspace?.id, workspaceUrl]);

  const messages = useMemo(() => state.messages.map(toThreadMessage), [state.messages]);
  const sendMessage = useCallback(
    async (content: string, attachments: MessageAttachment[] = []) => {
      if (!content.trim() && attachments.length === 0) return;
      setError(undefined);
      const response = await fetch(workspaceUrl("/messages"), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content, attachments })
      });
      if (!response.ok) throw new Error(await readResponseError(response));
      setState((await response.json()) as AppState);
    },
    [workspaceUrl]
  );
  const cancelTurn = useCallback(async () => {
    const response = await fetch(workspaceUrl("/cancel"), { method: "POST" });
    if (response.ok) setState((await response.json()) as AppState);
  }, [workspaceUrl]);
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages,
    isRunning: state.isRunning,
    convertMessage: (message) => message,
    onNew: async (message) => {
      const content = appendMessageText(message);
      await sendMessage(content);
    },
    onCancel: cancelTurn
  });

  const toolActions = useMemo(() => parseToolActions(state.events), [state.events]);
  const timeline = useMemo<TimelineItem[]>(() => {
    const items: TimelineItem[] = [];
    for (const message of state.messages) {
      items.push({ key: `m-${message.id}`, at: Date.parse(message.createdAt), kind: "message", message });
    }
    for (const action of toolActions) {
      items.push({ key: `t-${action.id}`, at: Date.parse(action.createdAt), kind: "tool", action });
    }
    items.sort((a, b) => a.at - b.at);
    return items;
  }, [state.messages, toolActions]);

  const viewportRef = useRef<HTMLDivElement>(null);
  const streamRef = useRef<HTMLDivElement>(null);
  const shouldFollowOutput = useRef(true);

  const handleThreadScroll = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    shouldFollowOutput.current = distanceFromBottom <= 48;
  }, []);

  useEffect(() => {
    const viewport = viewportRef.current;
    const stream = streamRef.current;
    if (!viewport || !stream) return;

    const followOutput = () => {
      if (shouldFollowOutput.current) viewport.scrollTop = viewport.scrollHeight;
    };
    const observer = new ResizeObserver(followOutput);
    observer.observe(stream);
    followOutput();
    return () => observer.disconnect();
  }, [state.session?.path, selectedWorkspace?.id]);

  useEffect(() => {
    shouldFollowOutput.current = true;
    const el = viewportRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [state.session?.path, selectedWorkspace?.id]);

  // Auto-open the preview panel only once a live preview/website becomes available.
  const previewCount = state.previews.length;
  const prevPreviewCount = useRef(0);
  useEffect(() => {
    if (previewCount > 0 && prevPreviewCount.current === 0) {
      setRightPanelOpen(true);
    }
    prevPreviewCount.current = previewCount;
  }, [previewCount]);

  async function newSession() {
    setError(undefined);
    const response = await fetch(workspaceUrl("/sessions"), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ new: true })
    });
    if (!response.ok) {
      setError(await readResponseError(response));
      return;
    }
    setState((await response.json()) as AppState);
  }

  async function openSession(session: SessionSummary) {
    setError(undefined);
    const response = await fetch(workspaceUrl("/sessions"), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: session.path })
    });
    if (!response.ok) {
      setError(await readResponseError(response));
      return;
    }
    setState((await response.json()) as AppState);
  }

  async function removePreview(preview: PreviewService) {
    const response = await fetch(`${workspaceUrl("/previews")}/${encodeURIComponent(preview.id)}`, { method: "DELETE" });
    setState((await response.json()) as AppState);
  }

  async function logout() {
    await fetch("/api/auth/logout", { method: "POST" });
    setMe(undefined);
    setState(emptyState);
    setSelectedWorkspaceId(undefined);
  }

  if (!authChecked) {
    return <LoadingScreen text="Loading Agent Mom" />;
  }

  if (authEnabled && !me) {
    return <AuthScreen onAuth={setMe} onAuthEnabled={setAuthEnabled} />;
  }

  if (!me || !selectedWorkspace) {
    return <LoadingScreen text="Preparing workspace" error={error} onRetry={() => void loadMe()} />;
  }

  if (isAdminPage) {
    return <AdminPage authEnabled={authEnabled} me={me} onLogout={logout} />;
  }

  if (isTelegramSettingsPage) {
    return <TelegramSettingsPage authEnabled={authEnabled} me={me} onLogout={logout} />;
  }

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <div className="app-shell">
        <aside className="sidebar">
          <div className="brand brand-lg">
            <div>
              <h1>Agent Mom</h1>
              <p>{me.user.fullName}</p>
            </div>
          </div>

          <div className="actions">
            <button type="button" disabled={state.isRunning} onClick={() => void newSession()}>
              <SquarePen size={16} />
              <span>New chat</span>
            </button>
            {me.user.role === "admin" && (
              <a className="action-link" href="/admin">
                <UserPlus size={16} />
                <span>Admin</span>
              </a>
            )}
            <a className="action-link" href="/settings/telegram">
              <MessageCircle size={16} />
              <span>Telegram</span>
            </a>
            {authEnabled && (
              <button type="button" onClick={() => void logout()}>
                <LogOut size={16} />
                <span>Logout</span>
              </button>
            )}
          </div>

          <section className="sessions">
            <h2>Sessions</h2>
            <div className="session-list">
              {state.sessions.length === 0 ? (
                <p className="muted">No persisted sessions yet.</p>
              ) : (
                state.sessions.map((session) => (
                  <button
                    type="button"
                    className={session.path === state.session?.path ? "session active" : "session"}
                    disabled={state.isRunning}
                    key={session.path ?? session.id}
                    onClick={() => void openSession(session)}
                  >
                    <GitBranch size={14} />
                    <span>{session.firstMessage || session.name || session.id}</span>
                  </button>
                ))
              )}
            </div>
          </section>
        </aside>

        <main className="main">
          {!rightPanelOpen && (
            <button
              type="button"
              className="panel-reopen"
              onClick={() => setRightPanelOpen(true)}
              title="Show panel"
            >
              <PanelRightOpen size={17} />
            </button>
          )}

          {(error || state.error) && <div className="error-line">{error ?? state.error}</div>}

          <div className={rightPanelOpen ? "content-grid" : "content-grid right-panel-closed"}>
            <section className="thread-panel">
              <ThreadPrimitive.Root className="thread-root">
                <div className="thread-viewport" ref={viewportRef} onScroll={handleThreadScroll}>
                  <div className="thread-stream" ref={streamRef}>
                    {timeline.length === 0 ? (
                      <div className="empty-thread">
                        <h2>Ask Pi to work in this workspace.</h2>
                        <p>Messages go straight to Pi. Keep the loop simple and inspect what changes.</p>
                      </div>
                    ) : (
                      timeline.map((item) =>
                        item.kind === "message" ? (
                          <MessageRow key={item.key} message={item.message} />
                        ) : (
                          <ToolCard key={item.key} action={item.action} />
                        )
                      )
                    )}
                    {state.isRunning && (
                      <div className="thinking" aria-label="Pi is thinking">
                        <Lightbulb size={16} className="thinking-icon" />
                        <span>Thinking…</span>
                      </div>
                    )}
                  </div>
                </div>

                <AttachmentComposer isRunning={state.isRunning} onCancel={cancelTurn} onSend={sendMessage} />
              </ThreadPrimitive.Root>
            </section>

            <div className="right-panel-slot" hidden={!rightPanelOpen}>
              <RightPanel
                previews={state.previews}
                onCollapse={() => setRightPanelOpen(false)}
                onRemovePreview={removePreview}
              />
            </div>
          </div>
        </main>
      </div>
    </AssistantRuntimeProvider>
  );
}

type ToolAction = {
  id: string;
  tool: string;
  createdAt: string;
  path?: string;
  command?: string;
  added?: number;
  removed?: number;
  detail?: string;
};

type TimelineItem =
  | { key: string; at: number; kind: "message"; message: ChatMessage }
  | { key: string; at: number; kind: "tool"; action: ToolAction };

const TOOL_LABELS: Record<string, string> = {
  read: "Read",
  write: "Wrote",
  edit: "Edited",
  bash: "Ran"
};

function parseToolActions(events: UiEvent[]): ToolAction[] {
  const actions: ToolAction[] = [];
  for (const event of events) {
    if (event.type !== "tool") continue;
    const match = /^([A-Za-z][\w-]*)\s+started$/.exec(event.title);
    if (!match) continue;
    const tool = match[1];
    let args: Record<string, unknown> = {};
    try {
      args = event.detail ? (JSON.parse(event.detail) as Record<string, unknown>) : {};
    } catch {
      args = {};
    }
    const action: ToolAction = { id: event.id, tool, createdAt: event.createdAt };
    if (typeof args.path === "string") action.path = args.path;
    if (typeof args.command === "string") action.command = args.command;
    if (tool === "write" && typeof args.content === "string") {
      action.added = countLines(args.content);
      action.detail = args.content;
    }
    if (tool === "edit") {
      const oldText = pickString(args, ["old_string", "oldText", "old"]);
      const newText = pickString(args, ["new_string", "newText", "new"]);
      if (oldText !== undefined) action.removed = countLines(oldText);
      if (newText !== undefined) action.added = countLines(newText);
      action.detail = newText ?? oldText;
    }
    if (tool === "bash") action.detail = action.command;
    actions.push(action);
  }
  return actions;
}

function pickString(obj: Record<string, unknown>, keys: string[]): string | undefined {
  for (const key of keys) {
    if (typeof obj[key] === "string") return obj[key] as string;
  }
  return undefined;
}

function countLines(text: string): number {
  if (!text) return 0;
  return text.replace(/\n$/, "").split("\n").length;
}

function basename(path: string | undefined): string {
  if (!path) return "";
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

function MessageRow({ message }: { message: ChatMessage }) {
  if (message.role === "user") {
    return (
      <div className="msg-row user">
        <div className="bubble user-bubble">
          {message.content}
          <MessageAttachments attachments={message.attachments ?? []} />
        </div>
      </div>
    );
  }
  return (
    <div className="msg-row assistant">
      <div className="assistant-body">
        <Markdown
          remarkPlugins={[remarkGfm]}
          components={{
            a: ({ node: _node, ...props }) => <a {...props} target="_blank" rel="noreferrer" />
          }}
        >
          {message.content}
        </Markdown>
        <MessageAttachments attachments={message.attachments ?? []} />
      </div>
    </div>
  );
}

function ToolCard({ action }: { action: ToolAction }) {
  const [open, setOpen] = useState(false);
  const label = TOOL_LABELS[action.tool] ?? action.tool;
  const isBash = action.tool === "bash";
  const title = isBash ? action.command ?? "command" : basename(action.path) || label;
  const subtitle = isBash ? undefined : action.path;
  const Icon =
    action.tool === "bash"
      ? Terminal
      : action.tool === "edit"
        ? FilePen
        : action.tool === "write"
          ? FilePlus2
          : FileText;

  return (
    <div className="tool-card">
      <button type="button" className="tool-card-head" onClick={() => setOpen((value) => !value)}>
        <span className="tool-card-icon">
          <Icon size={16} />
        </span>
        <span className="tool-card-text">
          <strong>
            <span className="tool-card-label">{label}</span> {title}
          </strong>
          {(subtitle || action.added != null || action.removed != null) && (
            <small>
              {subtitle}
              {action.added != null && <span className="diff-add"> +{action.added}</span>}
              {action.removed != null && <span className="diff-del"> -{action.removed}</span>}
            </small>
          )}
        </span>
        {action.detail && (
          <span className="tool-card-open">
            {open ? "Close" : "Open"}
            <ChevronDown size={13} className={open ? "chev open" : "chev"} />
          </span>
        )}
      </button>
      {open && action.detail && <pre className="tool-card-detail">{action.detail}</pre>}
    </div>
  );
}

function toThreadMessage(message: ChatMessage): ThreadMessageLike {
  return {
    id: message.id,
    role: message.role,
    content: message.content,
    createdAt: new Date(message.createdAt),
    status:
      message.role === "assistant"
        ? message.status === "running"
          ? { type: "running" }
          : message.status === "error"
            ? { type: "incomplete", reason: "error" }
            : { type: "complete", reason: "stop" }
        : undefined
  };
}

function appendMessageText(message: AppendMessage): string {
  const content = message.content;
  if (typeof content === "string") return content;
  return content
    .map((part) => (part.type === "text" ? part.text : ""))
    .filter(Boolean)
    .join("\n");
}

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
