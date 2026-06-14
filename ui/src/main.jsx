import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  Edit3,
  ExternalLink,
  GitBranch,
  MessageSquare,
  Monitor,
  PanelLeft,
  Plus,
  RefreshCcw,
  Send,
  Sparkles,
  Terminal,
  Trash2,
  Users,
  X,
} from 'lucide-react';
import { buildPendingPermissions, buildTranscript } from './acp/transcript.js';
import './styles.css';

const API_BASE = import.meta.env.VITE_API_BASE ?? '/api';

function Root() {
  const [userSession, setUserSession] = useState(null);
  const [checkingSession, setCheckingSession] = useState(true);

  useEffect(() => {
    validateSession()
      .then((session) => {
        setUserSession(session);
      })
      .catch(() => setUserSession(null))
      .finally(() => {
        setCheckingSession(false);
      });
  }, []);

  useEffect(() => {
    if (!userSession?.id) return undefined;

    let cancelled = false;
    const checkSession = async () => {
      try {
        const refreshedSession = await validateSession();
        if (!cancelled) {
          setUserSession(refreshedSession);
        }
      } catch {
        if (!cancelled) {
          setUserSession(null);
        }
      }
    };

    const interval = window.setInterval(checkSession, 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [userSession?.id]);

  function enterUserFlow(session) {
    setUserSession(session);
  }

  if (checkingSession) {
    return (
      <main className="landingPage">
        <section className="landingPanel" aria-label="User access">
          <BuildersTableBrand />
        </section>
      </main>
    );
  }

  if (!userSession) {
    return <LandingPage onSubmit={enterUserFlow} />;
  }

  if (window.location.pathname === '/admin') {
    if (userSession.role !== 'admin') {
      window.history.replaceState({}, '', '/');
    } else {
      return <AdminPage userSession={userSession} />;
    }
  }

  if (window.location.pathname === '/tui') {
    return <TuiPage userSession={userSession} />;
  }

  if (!userSession.userName || !userSession.agentName || !userSession.workspaceName) {
    return <SetupPage userSession={userSession} onSubmit={enterUserFlow} />;
  }

  return <App userSession={userSession} />;
}

function LandingPage({ onSubmit }) {
  const [form, setForm] = useState({ email: '', accessCode: '' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  async function submitAccess(event) {
    event.preventDefault();
    const email = form.email.trim();
    const accessCode = form.accessCode.trim();
    if (!email) return;

    setBusy(true);
    setError('');
    try {
      const response = await fetch(`${API_BASE}/auth/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          email,
          ...(accessCode ? { access_code: accessCode } : {}),
        }),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      onSubmit(sessionFromMe(data));
    } catch (accessError) {
      setError(accessError?.error ?? 'Access denied.');
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="landingPage">
      <section className="landingPanel" aria-label="User access">
        <BuildersTableBrand />

        <form className="landingForm" onSubmit={submitAccess}>
          <input
            type="email"
            value={form.email}
            onChange={(event) =>
              setForm((current) => ({ ...current, email: event.target.value }))
            }
            placeholder="Email"
            autoComplete="email"
            autoFocus
            required
          />
          <input
            value={form.accessCode}
            onChange={(event) =>
              setForm((current) => ({ ...current, accessCode: event.target.value }))
            }
            placeholder="Access Code"
            autoComplete="one-time-code"
          />
          {error && <p className="accessError">{error}</p>}
          <button disabled={busy || !form.email.trim()}>
            {busy ? 'Checking...' : 'Continue'}
          </button>
        </form>
      </section>
    </main>
  );
}

function BuildersTableBrand() {
  return (
    <div className="landingHeader">
      <h1>
        <span className="terminalPrompt" aria-label="Let's start building with...">
          <span className="typedPrompt" aria-hidden="true">
            $ Let's start building with...
          </span>
        </span>
        <strong>Agent Mom</strong>
      </h1>
    </div>
  );
}

async function validateSession() {
  return fetchMe();
}

async function fetchMe() {
  const response = await fetch(`${API_BASE}/me`);
  const data = await response.json();
  if (!response.ok) {
    throw data;
  }
  return sessionFromMe(data);
}

function SetupPage({ userSession, onSubmit }) {
  const [form, setForm] = useState({
    userName: userSession.userName ?? '',
    agentName: userSession.agentName ?? '',
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  async function submitSetup(event) {
    event.preventDefault();
    const userName = form.userName.trim();
    const agentName = form.agentName.trim();
    if (!userName || !agentName) return;

    setBusy(true);
    setError('');
    try {
      const session = await createWorkspaceFromOnboarding(userName, agentName);
      onSubmit(session);
    } catch (setupError) {
      setError(formatError(setupError));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="landingPage">
      <section className="landingPanel setupPanel" aria-label="Create your workspace">
        <BuildersTableBrand />

        <form className="landingForm setupForm" onSubmit={submitSetup}>
          <input
            value={form.userName}
            onChange={(event) =>
              setForm((current) => ({ ...current, userName: event.target.value }))
            }
            placeholder="Name"
            autoComplete="name"
            autoFocus
            required
          />
          <input
            value={form.agentName}
            onChange={(event) =>
              setForm((current) => ({ ...current, agentName: event.target.value }))
            }
            placeholder="Agent name"
            required
          />
          {error && <p className="accessError">{error}</p>}
          <button disabled={busy || !form.userName.trim() || !form.agentName.trim()}>
            {busy ? 'Creating workspace...' : 'Continue'}
          </button>
        </form>
      </section>
    </main>
  );
}

function App({ userSession }) {
  const [workspaces, setWorkspaces] = useState([]);
  const [selectedName, setSelectedName] = useState(userSession.workspaceName ?? '');
  const [busy, setBusy] = useState(false);
  const [chatBusy, setChatBusy] = useState(false);
  const [chatInput, setChatInput] = useState('');
  const [promptAttachments, setPromptAttachments] = useState([]);
  const [acpByWorkspace, setAcpByWorkspace] = useState({});
  const [chats, setChats] = useState([]);
  const [activeChatId, setActiveChatId] = useState('');
  const [chatStates, setChatStates] = useState({});
  const [workspaceError, setWorkspaceError] = useState('');
  const [previewsByWorkspace, setPreviewsByWorkspace] = useState({});
  const [activePreviewName, setActivePreviewName] = useState('');
  const [previewOpen, setPreviewOpen] = useState(false);
  const [previewError, setPreviewError] = useState('');
  const [previewReloadKey, setPreviewReloadKey] = useState(0);
  const [wakingWorkspaces, setWakingWorkspaces] = useState({});
  const [now, setNow] = useState(() => Date.now());
  const chatSocketsRef = useRef({});
  const chatsRef = useRef([]);
  const chatStatesRef = useRef({});
  const activeChatIdRef = useRef('');
  const fileInputRef = useRef(null);
  const pendingRpcRef = useRef({});
  const rpcIdRef = useRef(1);

  const selectedWorkspace = useMemo(
    () => workspaces.find((workspace) => workspace.name === selectedName) ?? workspaces[0],
    [selectedName, workspaces],
  );
  const workspaceReady = isWorkspaceReady(selectedWorkspace);
  const selectedWorkspaceName = selectedWorkspace?.name ?? selectedName;
  const workspaceWaking = selectedWorkspaceName ? Boolean(wakingWorkspaces[selectedWorkspaceName]) : false;
  const acp = selectedWorkspaceName ? acpByWorkspace[selectedWorkspaceName] ?? emptyAcpState() : emptyAcpState();
  const workspaceChats = useMemo(
    () => chats.filter((chat) => chat.workspaceName === selectedWorkspace?.name),
    [chats, selectedWorkspace?.name],
  );
  const activeChat =
    workspaceChats.find((chat) => chat.id === activeChatId) ?? workspaceChats[0] ?? null;
  const activeChatState = activeChat ? chatStates[activeChat.id] ?? emptyChatState() : emptyChatState();
  const promptCapabilities = acp.capabilities?.promptCapabilities ?? acp.capabilities?.prompt_capabilities ?? {};
  const sessionCapabilities = acp.capabilities?.sessionCapabilities ?? acp.capabilities?.session_capabilities ?? {};
  const modelState = activeChatState.models ?? null;
  const modeState = activeChatState.modes ?? null;
  const configOptions = activeChatState.configOptions ?? [];
  const chatReady = acp.state === 'ready' && Boolean(activeChatState.session_id);
  const transcript = useMemo(() => buildTranscript(activeChatState.events), [activeChatState.events]);
  const pendingPermissions = useMemo(
    () => buildPendingPermissions(activeChatState.events),
    [activeChatState.events],
  );
  const chatGroups = groupChatsByAge(
    workspaceChats,
    now,
  );
  const workspacePreviews = selectedWorkspaceName ? previewsByWorkspace[selectedWorkspaceName] ?? [] : [];
  const activePreview =
    workspacePreviews.find((preview) => preview.name === activePreviewName) ??
    workspacePreviews[0] ??
    null;

  useEffect(() => {
    chatStatesRef.current = chatStates;
  }, [chatStates]);

  useEffect(() => {
    chatsRef.current = chats;
  }, [chats]);

  useEffect(() => {
    activeChatIdRef.current = activeChatId;
  }, [activeChatId]);

  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(interval);
  }, []);

  useEffect(() => {
    refresh().catch((error) => setWorkspaceError(formatError(error)));
  }, []);

  useEffect(() => {
    if (!selectedWorkspace || workspaceReady) return undefined;
    const interval = window.setInterval(() => {
      refresh().catch((error) => setWorkspaceError(formatError(error)));
    }, 3_000);
    return () => window.clearInterval(interval);
  }, [selectedWorkspace?.name, selectedWorkspace?.status, workspaceReady]);

  useEffect(() => {
    if (!selectedWorkspace?.name || workspaceReady || workspaceWaking) return;
    if (!isWorkspaceStartable(selectedWorkspace)) return;
    wakeWorkspace(selectedWorkspace.name).catch((error) => setWorkspaceError(formatError(error)));
  }, [selectedWorkspace?.name, selectedWorkspace?.status, workspaceReady, workspaceWaking]);

  useEffect(() => {
    if (!selectedWorkspace?.name) return;
    if (!activeChat || activeChat.workspaceName !== selectedWorkspace.name) {
      setActiveChatId(workspaceChats[0]?.id ?? '');
    }
  }, [selectedWorkspace?.name, chats.length]);

  useEffect(() => {
    if (!selectedWorkspace?.name || !activeChatId) return;
    if (acp.state !== 'ready' && acp.state !== 'open') return;
    ensureChatSession(selectedWorkspace.name, activeChatId);
  }, [selectedWorkspace?.name, activeChatId, acp.state]);

  useEffect(() => {
    if (!selectedWorkspaceName || !previewOpen) return undefined;
    let cancelled = false;
    const pollPreviews = async () => {
      try {
        await loadPreviews(selectedWorkspaceName);
      } catch (error) {
        if (!cancelled) {
          setPreviewError(formatError(error));
        }
      }
    };
    pollPreviews();
    const interval = window.setInterval(pollPreviews, 4_000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [selectedWorkspaceName, previewOpen]);

  useEffect(() => {
    if (!workspacePreviews.length) {
      setActivePreviewName('');
      return;
    }
    if (!workspacePreviews.some((preview) => preview.name === activePreviewName)) {
      setActivePreviewName(workspacePreviews[0].name);
    }
  }, [selectedWorkspaceName, workspacePreviews.length, activePreviewName]);

  useEffect(() => {
    if (!selectedWorkspace?.name || !workspaceReady) return undefined;

    const workspaceName = selectedWorkspace.name;
    const socket = new WebSocket(chatWsUrl(workspaceName));
    let cancelled = false;
    let terminalStatusReceived = false;

    chatSocketsRef.current[workspaceName]?.close();
    chatSocketsRef.current[workspaceName] = socket;
    setAcpConnectionState(workspaceName, {
      state: 'connecting',
      phase: 'websocket',
      error: null,
    });

    socket.onopen = () => {
      if (cancelled) return;
      setAcpConnectionState(workspaceName, { state: 'initializing', phase: 'initialize', error: null });
      sendRpc(workspaceName, null, 'initialize', {
        protocolVersion: 1,
        clientCapabilities: {},
        clientInfo: { name: 'agent-mom', version: '0.1.1' },
      });
    };

    socket.onmessage = (event) => {
      if (cancelled) return;
      const message = parseJsonMessage(event.data);
      const responseKey = message.id != null ? rpcKey(workspaceName, message.id) : null;
      const pendingRpc = responseKey ? pendingRpcRef.current[responseKey] : null;
      const eventChatId = chatIdForAcpMessage(message, pendingRpc?.chatId);
      if (eventChatId) {
        appendChatEvent(eventChatId, 'in', message, pendingRpc?.method ? { rpcMethod: pendingRpc.method } : {});
        const title = titleFromSessionUpdate(message);
        if (title) {
          touchChat(eventChatId, { title });
        }
      }

      if (message.method === 'mom/status') {
        const params = message.params ?? {};
        if (params.state === 'error') {
          terminalStatusReceived = true;
          setAcpConnectionState(workspaceName, {
            state: 'failed',
            phase: 'transport',
            error: params.message ?? null,
          });
          return;
        }
        setAcpConnectionState(workspaceName, {
          phase: params.state ?? 'transport',
          error: params.message ?? null,
        });
        return;
      }

      if (message.id != null) {
        const key = responseKey ?? rpcKey(workspaceName, message.id);
        const rpc = pendingRpcRef.current[key];
        delete pendingRpcRef.current[key];
        const method = rpc?.method;
        const chatId = rpc?.chatId;

        if (message.error) {
          terminalStatusReceived = true;
          if (method === 'session/prompt') {
            setChatBusy(false);
          }
          if (method === 'session/set_model' && chatId) {
            setChatState(chatId, { modelChanging: false });
          }
          if (method === 'session/set_mode' && chatId) {
            setChatState(chatId, { modeChanging: false });
          }
          if (method === 'session/set_config_option' && chatId) {
            setChatState(chatId, { configChanging: false });
          }
          if (method === 'session/fork' && chatId) {
            setChatState(chatId, { creatingSession: false });
          }
          setAcpConnectionState(workspaceName, {
            state: 'failed',
            phase: method ?? 'rpc',
            error: JSON.stringify(message.error),
          });
          return;
        }

        if (method === 'initialize') {
          setAcpConnectionState(workspaceName, {
            state: 'open',
            phase: 'session/list',
            capabilities: message.result?.agentCapabilities ?? message.result?.agent_capabilities ?? {},
            agentInfo: message.result?.agentInfo ?? message.result?.agent_info ?? null,
            authMethods: message.result?.authMethods ?? message.result?.auth_methods ?? [],
          });
          sendRpc(workspaceName, null, 'session/list', { cwd: '/workspace' });
        } else if (method === 'session/new') {
          const sessionId = message.result?.sessionId ?? message.result?.session_id;
          if (chatId) {
            promoteChatSession(workspaceName, chatId, sessionId, message.result);
          }
          setAcpConnectionState(workspaceName, {
            state: sessionId ? 'ready' : 'open',
            phase: 'ready',
          });
        } else if (method === 'session/list') {
          applySessionList(workspaceName, message.result);
        } else if (method === 'session/load' || method === 'session/resume') {
          if (method === 'session/load' && !message.result && chatId) {
            setAcpConnectionState(workspaceName, { state: 'open', phase: 'session/resume' });
            sendRpc(workspaceName, chatId, 'session/resume', {
              cwd: '/workspace',
              sessionId: chatStatesRef.current[chatId]?.session_id ?? chatId,
              mcpServers: [],
            });
            return;
          }
          if (chatId) {
            setChatState(chatId, {
              loadingSession: false,
              loaded: true,
              models: message.result?.models ?? null,
              modes: message.result?.modes ?? null,
              configOptions: message.result?.configOptions ?? message.result?.config_options ?? [],
            });
          }
          setAcpConnectionState(workspaceName, { state: 'ready', phase: 'ready' });
        } else if (method === 'session/fork') {
          const sessionId = message.result?.sessionId ?? message.result?.session_id;
          if (chatId) {
            promoteChatSession(workspaceName, chatId, sessionId, message.result);
          }
          setAcpConnectionState(workspaceName, {
            state: sessionId ? 'ready' : 'open',
            phase: 'ready',
          });
        } else if (method === 'session/close') {
          if (chatId) {
            removeChat(workspaceName, chatId);
          }
          setAcpConnectionState(workspaceName, { state: 'open', phase: 'session/list' });
          sendRpc(workspaceName, null, 'session/list', { cwd: '/workspace' });
        } else if (method === 'session/set_model') {
          if (chatId) {
            setChatState(chatId, { modelChanging: false });
          }
        } else if (method === 'session/set_mode') {
          if (chatId) {
            setChatState(chatId, { modeChanging: false });
          }
        } else if (method === 'session/set_config_option') {
          if (chatId) {
            const previousOptions = chatStatesRef.current[chatId]?.configOptions ?? [];
            setChatState(chatId, {
              configChanging: false,
              configOptions: message.result?.configOptions ?? message.result?.config_options ?? previousOptions,
            });
          }
        } else if (method === 'session/prompt') {
          setChatBusy(false);
          sendRpc(workspaceName, null, 'session/list', { cwd: '/workspace' });
        }
      }
    };

    socket.onerror = () => {
      if (cancelled) return;
      terminalStatusReceived = true;
      setChatBusy(false);
      setAcpConnectionState(workspaceName, {
        state: 'failed',
        phase: 'websocket',
        error: 'Hermes ACP websocket failed.',
      });
    };

    socket.onclose = () => {
      if (cancelled || terminalStatusReceived) return;
      setChatBusy(false);
      setAcpConnectionState(workspaceName, { state: 'exited', phase: 'closed' });
    };

    return () => {
      cancelled = true;
      if (chatSocketsRef.current[workspaceName] === socket) {
        delete chatSocketsRef.current[workspaceName];
      }
      socket.close();
    };
  }, [selectedWorkspace?.name, workspaceReady]);

  async function request(path, options = {}) {
    setBusy(true);
    try {
      return await apiRequest(path, options, userSession);
    } finally {
      setBusy(false);
    }
  }

  function createChat(workspaceName, options = {}) {
    const timestamp = Date.now();
    const chat = {
      id: newChatId(workspaceName),
      workspaceName,
      title: options.title ?? 'New chat',
      sessionId: null,
      acpSessionId: null,
      temporary: true,
      createdAt: timestamp,
      updatedAt: timestamp,
    };
    setChats((current) => {
      const next = [chat, ...current];
      chatsRef.current = next;
      return next;
    });
    setChatStates((current) => {
      const next = {
        ...current,
        [chat.id]: {
          ...emptyChatState(),
          ...(options.initialState ?? {}),
        },
      };
      chatStatesRef.current = next;
      return next;
    });
    if (options.select) {
      activeChatIdRef.current = chat.id;
      setActiveChatId(chat.id);
    }
    return chat;
  }

  function applySessionList(workspaceName, result = {}) {
    const timestamp = Date.now();
    const sessionChats = normalizeAcpSessions(workspaceName, result);
    const current = chatsRef.current;
    let nextActiveId = activeChatIdRef.current;
    const nextStates = { ...chatStatesRef.current };
    const otherWorkspaces = current.filter((chat) => chat.workspaceName !== workspaceName);
    const temporaryChats = current.filter((chat) => chat.workspaceName === workspaceName && chat.temporary);
    const currentById = new Map(current.map((chat) => [chat.id, chat]));
    const nextWorkspaceChats = sessionChats.map((chat) => ({
      ...currentById.get(chat.id),
      ...chat,
    }));
    const next = [...otherWorkspaces, ...temporaryChats, ...nextWorkspaceChats]
      .sort((left, right) => (right.updatedAt ?? 0) - (left.updatedAt ?? 0));

    if (!next.some((chat) => chat.id === nextActiveId && chat.workspaceName === workspaceName)) {
      nextActiveId = nextWorkspaceChats[0]?.id ?? temporaryChats[0]?.id ?? '';
    }
    chatsRef.current = next;
    setChats(next);

    for (const chat of sessionChats) {
      nextStates[chat.id] = {
        ...emptyChatState(),
        ...(nextStates[chat.id] ?? {}),
        session_id: chat.sessionId,
        listedAt: timestamp,
      };
    }
    chatStatesRef.current = nextStates;
    setChatStates(nextStates);

    activeChatIdRef.current = nextActiveId;
    setActiveChatId(nextActiveId);
    setAcpConnectionState(workspaceName, {
      state: nextActiveId ? 'open' : 'ready',
      phase: nextActiveId ? 'session/load' : 'ready',
    });

    if (nextActiveId) {
      loadChatSession(workspaceName, nextActiveId);
    }
  }

  function promoteChatSession(workspaceName, chatId, sessionId, result = {}) {
    if (!sessionId) {
      setChatState(chatId, { creatingSession: false, loadingSession: false });
      return;
    }

    const timestamp = Date.now();
    const previous = chatStatesRef.current[chatId] ?? emptyChatState();
    setChats((current) => {
      const next = current.map((chat) =>
        chat.id === chatId
          ? {
              ...chat,
              id: sessionId,
              sessionId,
              acpSessionId: sessionId,
              temporary: false,
              title: sessionTitle(result) || chat.title,
              updatedAt: timestamp,
            }
          : chat,
      );
      chatsRef.current = next;
      return next;
    });
    setChatStates((current) => {
      const next = { ...current };
      delete next[chatId];
      next[sessionId] = {
        ...previous,
        session_id: sessionId,
        creatingSession: false,
        loadingSession: false,
        loaded: true,
        models: result.models ?? previous.models ?? null,
        modes: result.modes ?? previous.modes ?? null,
        configOptions: result.configOptions ?? result.config_options ?? previous.configOptions ?? [],
      };
      chatStatesRef.current = next;
      return next;
    });
    activeChatIdRef.current = sessionId;
    setActiveChatId(sessionId);
  }

  function touchChat(chatId, patch = {}) {
    setChats((current) => {
      const next = current.map((chat) =>
        chat.id === chatId
          ? {
              ...chat,
              ...patch,
              updatedAt: Date.now(),
            }
          : chat,
      );
      chatsRef.current = next;
      return next;
    });
  }

  function removeChat(workspaceName, chatId) {
    const current = chatsRef.current;
    const next = current.filter((chat) => chat.id !== chatId);
    const nextStates = { ...chatStatesRef.current };
    delete nextStates[chatId];

    chatsRef.current = next;
    chatStatesRef.current = nextStates;
    setChats(next);
    setChatStates(nextStates);

    if (activeChatIdRef.current === chatId) {
      const nextActiveId = next.find((chat) => chat.workspaceName === workspaceName)?.id ?? '';
      activeChatIdRef.current = nextActiveId;
      setActiveChatId(nextActiveId);
      if (nextActiveId) {
        loadChatSession(workspaceName, nextActiveId);
      }
    }
  }

  function appendChatEvent(chatId, direction, message, metadata = {}) {
    setChatStates((current) => {
      const previous = current[chatId] ?? emptyChatState();
      const seq = (previous.events.at(-1)?.seq ?? 0) + 1;
      const next = {
        ...current,
        [chatId]: {
          ...previous,
          events: [...previous.events, { seq, at: Date.now(), direction, message, ...metadata }],
          startedAt: previous.startedAt ?? Date.now(),
          updatedAt: Date.now(),
        },
      };
      chatStatesRef.current = next;
      return next;
    });
    touchChat(chatId);
  }

  function setChatState(chatId, patch) {
    setChatStates((current) => {
      const previous = current[chatId] ?? emptyChatState();
      const next = {
        ...current,
        [chatId]: {
          ...previous,
          ...patch,
          startedAt: previous.startedAt ?? Date.now(),
          updatedAt: Date.now(),
        },
      };
      chatStatesRef.current = next;
      return next;
    });
  }

  function setAcpConnectionState(workspaceName, patch) {
    setAcpByWorkspace((current) => {
      const previous = current[workspaceName] ?? emptyAcpState();
      return {
        ...current,
        [workspaceName]: {
          ...previous,
          ...patch,
          startedAt: previous.startedAt ?? Date.now(),
          updatedAt: Date.now(),
        },
      };
    });
  }

  function ensureChatSession(workspaceName, chatId) {
    if (!workspaceName || !chatId) return;
    const socket = chatSocketsRef.current[workspaceName];
    if (!socket || socket.readyState !== WebSocket.OPEN) return;

    const state = chatStatesRef.current[chatId] ?? emptyChatState();
    if (state.session_id || state.creatingSession) return;

    setChatState(chatId, { creatingSession: true });
    setAcpConnectionState(workspaceName, { state: 'open', phase: 'session/new', error: null });
    try {
      sendRpc(workspaceName, chatId, 'session/new', { cwd: '/workspace', mcpServers: [] });
    } catch (error) {
      setChatState(chatId, { creatingSession: false });
      setAcpConnectionState(workspaceName, {
        state: 'failed',
        phase: 'session/new',
        error: formatError(error),
      });
    }
  }

  function loadChatSession(workspaceName, chatId) {
    if (!workspaceName || !chatId) return;
    const socket = chatSocketsRef.current[workspaceName];
    if (!socket || socket.readyState !== WebSocket.OPEN) return;

    const state = chatStatesRef.current[chatId] ?? emptyChatState();
    const sessionId = state.session_id ?? chatId;
    if (!sessionId || state.loadingSession) return;
    if (state.loaded && (state.events ?? []).length > 0) {
      setAcpConnectionState(workspaceName, { state: 'ready', phase: 'ready' });
      return;
    }

    setChatState(chatId, {
      events: [],
      session_id: sessionId,
      loadingSession: true,
      loaded: false,
    });
    setAcpConnectionState(workspaceName, { state: 'open', phase: 'session/load', error: null });
    try {
      sendRpc(workspaceName, chatId, 'session/load', {
        cwd: '/workspace',
        sessionId,
        mcpServers: [],
      });
    } catch (error) {
      setChatState(chatId, { loadingSession: false });
      setAcpConnectionState(workspaceName, {
        state: 'failed',
        phase: 'session/load',
        error: formatError(error),
      });
    }
  }

  function sendRpc(workspaceName, chatId, method, params = {}) {
    const id = rpcIdRef.current++;
    const message = { jsonrpc: '2.0', id, method, params };
    pendingRpcRef.current[rpcKey(workspaceName, id)] = { method, chatId };
    sendAcpMessage(workspaceName, message, chatId);
    return id;
  }

  function sendAcpMessage(workspaceName, message, chatId = null) {
    const socket = chatSocketsRef.current[workspaceName];
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new Error('Hermes ACP websocket is not connected.');
    }
    socket.send(JSON.stringify(message));
    if (chatId) {
      appendChatEvent(chatId, 'out', message);
    }
  }

  function chatIdForAcpMessage(message, pendingChatId = null) {
    if (pendingChatId) return pendingChatId;

    const sessionId = extractSessionId(message);
    if (sessionId) {
      const match = Object.entries(chatStatesRef.current).find(
        ([, state]) => state.session_id === sessionId,
      );
      if (match) return match[0];
    }

    if (message.method === 'session/update' || message.method === 'session/request_permission') {
      return activeChatIdRef.current;
    }

    return null;
  }

  async function refresh() {
    setWorkspaceError('');
    const allWorkspaces = normalizeWorkspaceList(await request('/workspaces'));
    const userWorkspace = selectUserWorkspace(allWorkspaces, userSession);
    const nextWorkspaces = userSession.workspaceName
      ? userWorkspace
        ? [userWorkspace]
        : []
      : allWorkspaces;
    setWorkspaces(nextWorkspaces);

    if (userWorkspace) {
      setSelectedName(userWorkspace.name);
    } else if (userSession.workspaceName) {
      setSelectedName(userSession.workspaceName);
      setWorkspaceError(
        `Workspace ${userSession.workspaceDisplayName ?? userSession.workspaceName} was not returned by the backend.`,
      );
    } else if (nextWorkspaces.length && !nextWorkspaces.some((workspace) => workspace.name === selectedName)) {
      setSelectedName(nextWorkspaces[0].name);
    } else if (!nextWorkspaces.length) {
      setSelectedName('');
    }

    return nextWorkspaces;
  }

  async function loadPreviews(workspaceName) {
    if (!workspaceName) return [];
    const previews = await request(`/workspaces/${encodeURIComponent(workspaceName)}/previews`);
    setPreviewsByWorkspace((current) => ({
      ...current,
      [workspaceName]: previews,
    }));
    setPreviewError('');
    return previews;
  }

  async function togglePreviewPane() {
    const nextOpen = !previewOpen;
    setPreviewOpen(nextOpen);
    if (nextOpen && selectedWorkspaceName) {
      try {
        await loadPreviews(selectedWorkspaceName);
      } catch (error) {
        setPreviewError(formatError(error));
      }
    }
  }

  function reloadPreview() {
    setPreviewReloadKey((current) => current + 1);
  }

  async function removeActivePreview() {
    if (!selectedWorkspaceName || !activePreview) return;
    setPreviewError('');
    try {
      await request(
        `/workspaces/${encodeURIComponent(selectedWorkspaceName)}/previews/${encodeURIComponent(activePreview.name)}`,
        { method: 'DELETE' },
      );
      await loadPreviews(selectedWorkspaceName);
    } catch (error) {
      setPreviewError(formatError(error));
    }
  }

  async function wakeWorkspace(workspaceName) {
    setWakingWorkspaces((current) => ({ ...current, [workspaceName]: true }));
    setWorkspaceError('');
    try {
      await request(`/workspaces/${encodeURIComponent(workspaceName)}/start`, {
        method: 'POST',
      });
      await refresh();
    } finally {
      setWakingWorkspaces((current) => {
        const next = { ...current };
        delete next[workspaceName];
        return next;
      });
    }
  }

  function openAdminPage() {
    window.location.href = '/admin';
  }

  function openTuiPage() {
    window.location.href = '/tui';
  }

  async function sendMessage(event) {
    event.preventDefault();
    if (!selectedWorkspace || !activeChat) return;

    const prompt = chatInput.trim();
    if (!prompt && !promptAttachments.length) return;
    if (!activeChatState.session_id) {
      ensureChatSession(selectedWorkspace.name, activeChat.id);
      return;
    }

    setChatInput('');
    const attachments = promptAttachments;
    setPromptAttachments([]);
    setChatBusy(true);
    try {
      if (activeChat.title === 'New chat') {
        touchChat(activeChat.id, { title: chatTitle(prompt) });
      }
      sendRpc(selectedWorkspace.name, activeChat.id, 'session/prompt', {
        sessionId: activeChatState.session_id,
        messageId: `agent-mom-${Date.now()}`,
        prompt: promptBlocks(prompt, attachments),
      });
    } catch (error) {
      setPromptAttachments(attachments);
      setChatBusy(false);
      setAcpConnectionState(selectedWorkspace.name, {
        state: 'failed',
        phase: 'send',
        error: formatError(error),
      });
    }
  }

  async function restartChat() {
    if (!selectedWorkspace) return;
    if (acp.state !== 'ready') return;
    const chat = createChat(selectedWorkspace.name, { select: true });
    ensureChatSession(selectedWorkspace.name, chat.id);
  }

  function selectChat(chat) {
    activeChatIdRef.current = chat.id;
    setActiveChatId(chat.id);
    setPromptAttachments([]);
    if (chat.sessionId || chat.acpSessionId) {
      loadChatSession(chat.workspaceName, chat.id);
    } else {
      ensureChatSession(chat.workspaceName, chat.id);
    }
  }

  async function forkChat() {
    if (!selectedWorkspace || !activeChatState.session_id) return;
    const chat = createChat(selectedWorkspace.name, {
      select: true,
      title: `${activeChat?.title ?? 'Chat'} fork`,
      initialState: { creatingSession: true },
    });
    setAcpConnectionState(selectedWorkspace.name, { state: 'open', phase: 'session/fork', error: null });
    try {
      sendRpc(selectedWorkspace.name, chat.id, 'session/fork', {
        cwd: '/workspace',
        sessionId: activeChatState.session_id,
        mcpServers: [],
      });
    } catch (error) {
      setChatState(chat.id, { creatingSession: false });
      setAcpConnectionState(selectedWorkspace.name, {
        state: 'failed',
        phase: 'session/fork',
        error: formatError(error),
      });
    }
  }

  async function closeChat(chat, event) {
    event.stopPropagation();
    if (!selectedWorkspace || !chat) return;
    if (chat.temporary) {
      removeChat(chat.workspaceName, chat.id);
      return;
    }

    const state = chatStatesRef.current[chat.id] ?? emptyChatState();
    const sessionId = state.session_id ?? chat.sessionId ?? chat.acpSessionId ?? chat.id;
    if (!sessionId) return;

    setAcpConnectionState(chat.workspaceName, { state: 'open', phase: 'session/close', error: null });
    try {
      sendRpc(chat.workspaceName, chat.id, 'session/close', {
        sessionId,
        session_id: sessionId,
      });
    } catch (error) {
      setAcpConnectionState(chat.workspaceName, {
        state: 'failed',
        phase: 'session/close',
        error: formatError(error),
      });
    }
  }

  async function changeModel(modelId) {
    if (!selectedWorkspace || !activeChat || !activeChatState.session_id || !modelId) return;
    setChatState(activeChat.id, { modelChanging: true });
    const previousModels = activeChatState.models;
    setChatState(activeChat.id, {
      models: patchCurrentModel(previousModels, modelId),
    });
    try {
      sendRpc(selectedWorkspace.name, activeChat.id, 'session/set_model', {
        sessionId: activeChatState.session_id,
        modelId,
      });
    } catch (error) {
      setChatState(activeChat.id, { modelChanging: false, models: previousModels });
      setWorkspaceError(formatError(error));
    }
  }

  async function changeMode(modeId) {
    if (!selectedWorkspace || !activeChat || !activeChatState.session_id || !modeId) return;
    setChatState(activeChat.id, { modeChanging: true });
    const previousModes = activeChatState.modes;
    setChatState(activeChat.id, {
      modes: patchCurrentMode(previousModes, modeId),
    });
    try {
      sendRpc(selectedWorkspace.name, activeChat.id, 'session/set_mode', {
        sessionId: activeChatState.session_id,
        modeId,
      });
    } catch (error) {
      setChatState(activeChat.id, { modeChanging: false, modes: previousModes });
      setWorkspaceError(formatError(error));
    }
  }

  async function changeConfigOption(option, value) {
    if (!selectedWorkspace || !activeChat || !activeChatState.session_id || !option?.id) return;
    setChatState(activeChat.id, { configChanging: true });
    const previousOptions = activeChatState.configOptions;
    setChatState(activeChat.id, {
      configOptions: patchConfigOption(previousOptions, option.id, value),
    });
    try {
      sendRpc(selectedWorkspace.name, activeChat.id, 'session/set_config_option', {
        sessionId: activeChatState.session_id,
        configId: option.id,
        value: String(value),
      });
    } catch (error) {
      setChatState(activeChat.id, { configChanging: false, configOptions: previousOptions });
      setWorkspaceError(formatError(error));
    }
  }

  async function attachPromptFiles(event) {
    const files = [...(event.target.files ?? [])];
    event.target.value = '';
    if (!files.length) return;
    try {
      const attachments = await Promise.all(files.map(readImageAttachment));
      setPromptAttachments((current) => [...current, ...attachments]);
    } catch (error) {
      setWorkspaceError(formatError(error));
    }
  }

  async function cancelChat() {
    if (!selectedWorkspace || !activeChat) return;
    setChatBusy(true);
    try {
      sendAcpMessage(selectedWorkspace.name, {
        jsonrpc: '2.0',
        method: 'session/cancel',
        params: { sessionId: activeChatState.session_id },
      }, activeChat.id);
    } catch (error) {
      setWorkspaceError(formatError(error));
    } finally {
      setChatBusy(false);
    }
  }

  async function respondPermission(permission, optionId) {
    if (!selectedWorkspace || !activeChat) return;
    setChatBusy(true);
    try {
      sendAcpMessage(selectedWorkspace.name, {
        jsonrpc: '2.0',
        id: parseJsonRpcId(permission.id),
        result: permissionResult(optionId),
      }, activeChat.id);
    } catch (error) {
      setWorkspaceError(formatError(error));
    } finally {
      setChatBusy(false);
    }
  }

  async function launchHermes() {
    if (!selectedWorkspace) return;

    try {
      const result = await request(`/workspaces/${encodeURIComponent(selectedWorkspace.name)}/hermes-ui`, {
        method: 'POST',
      });
      const url = launchUrlFromResult(result);
      if (url) {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
      await refresh();
    } catch (error) {
      appendSystemMessage(formatError(error));
    }
  }

  function appendSystemMessage(content) {
    if (!selectedWorkspace || !activeChat) {
      setWorkspaceError(content);
      return;
    }
    appendChatEvent(activeChat.id, 'in', {
      jsonrpc: '2.0',
      method: 'mom/status',
      params: { state: 'error', message: content },
    });
  }

  return (
    <main className={`appShell ${previewOpen ? 'withPreview' : ''}`}>
      <aside className="sidebar">
        <div className="userDropdown">
          <div className="brandRow">
            <div className="brandMark">A</div>
            <div className="brandButton">Agent Mom</div>
          </div>
        </div>

        <div className="sidebarQuickActions">
          <button className="launchButton" onClick={restartChat} disabled={!selectedWorkspace || !workspaceReady || chatBusy || acp.state !== 'ready'}>
            <Edit3 size={24} strokeWidth={2.25} />
            New chat
          </button>
        </div>

        <div className="chatHistory">
          {chatGroups.map((group) => (
            <section className="chatHistoryGroup" key={group.label}>
              <h2>{group.label}</h2>
              <div className="chatHistoryList">
                {group.chats.map((chat) => (
                  <div
                    key={chat.id}
                    className={`chatHistoryItem ${chat.id === activeChat?.id ? 'active' : ''}`}
                  >
                    <button type="button" onClick={() => selectChat(chat)}>
                      <span>{chat.title}</span>
                    </button>
                    <button
                      type="button"
                      title="Close chat"
                      aria-label={`Close ${chat.title}`}
                      onClick={(event) => closeChat(chat, event)}
                      disabled={!workspaceReady || chatBusy || !['open', 'ready'].includes(acp.state)}
                    >
                      <Trash2 size={15} />
                    </button>
                  </div>
                ))}
              </div>
            </section>
          ))}
          {selectedWorkspace && !chatGroups.length && <p className="emptyList">New chats will appear here.</p>}
        </div>

        <div className="sessionBox">
          <h2>Session</h2>
          <strong>Local workspace</strong>
          <span>{userSession.email}</span>
          <code>{userSession.code}</code>
        </div>
      </aside>

      <section className="chatShell">
        <header className="chatHeader">
          <button className="squareButton" title="Toggle sidebar">
            <PanelLeft size={20} />
          </button>
          <div>
            <h1>{selectedWorkspace ? workspaceDisplayName(selectedWorkspace) : 'Agent workspace'}</h1>
            <p>
              {selectedWorkspace
                ? workspaceWaking
                  ? 'Starting'
                  : friendlyStatus(selectedWorkspace.status)
                : 'Create a workspace to begin.'}
            </p>
            {selectedWorkspace && (
              <small className={`acpStatus ${acp.state}`}>
                Hermes ACP: {acp.state}
                {acp.phase && ` (${acp.phase})`}
              </small>
            )}
          </div>
          <div className="headerActions">
            {modelState && (
              <select
                className="sessionSelect"
                value={modelState.currentModelId ?? modelState.current_model_id ?? ''}
                onChange={(event) => changeModel(event.target.value)}
                disabled={!chatReady || activeChatState.modelChanging}
                aria-label="Model"
              >
                {availableModels(modelState).map((model) => (
                  <option key={model.modelId} value={model.modelId}>
                    {model.name}
                  </option>
                ))}
              </select>
            )}
            {modeState && (
              <select
                className="sessionSelect"
                value={modeState.currentModeId ?? modeState.current_mode_id ?? ''}
                onChange={(event) => changeMode(event.target.value)}
                disabled={!chatReady || activeChatState.modeChanging}
                aria-label="Mode"
              >
                {availableModes(modeState).map((mode) => (
                  <option key={mode.modeId} value={mode.modeId}>
                    {mode.name}
                  </option>
                ))}
              </select>
            )}
            {configOptions.map((option) =>
              option.type === 'boolean' ? (
                <label className="sessionToggle" key={option.id} title={option.description ?? option.name}>
                  <input
                    type="checkbox"
                    checked={Boolean(option.currentValue ?? option.current_value)}
                    onChange={(event) => changeConfigOption(option, event.target.checked)}
                    disabled={!chatReady || activeChatState.configChanging}
                  />
                  <span>{option.name ?? option.id}</span>
                </label>
              ) : (
                <select
                  className="sessionSelect"
                  key={option.id}
                  value={option.currentValue ?? option.current_value ?? ''}
                  onChange={(event) => changeConfigOption(option, event.target.value)}
                  disabled={!chatReady || activeChatState.configChanging}
                  aria-label={option.name ?? option.id}
                  title={option.description ?? option.name ?? option.id}
                >
                  {configSelectOptions(option).map((choice) => (
                    <option key={choice.value} value={choice.value}>
                      {choice.label}
                    </option>
                  ))}
                </select>
              ),
            )}
            {userSession.role === 'admin' && (
              <button className="refreshButton" type="button" onClick={openAdminPage}>
                <Users size={17} />
                Admin
              </button>
            )}
            <button className="refreshButton" onClick={refresh} disabled={busy}>
              <RefreshCcw size={17} />
              Refresh
            </button>
            <button className="refreshButton" onClick={togglePreviewPane} disabled={!selectedWorkspace || busy}>
              <Monitor size={17} />
              Preview
            </button>
            <button className="refreshButton" onClick={launchHermes} disabled={!selectedWorkspace || !workspaceReady || busy}>
              <ExternalLink size={17} />
              Hermes
            </button>
            <button className="refreshButton" type="button" onClick={openTuiPage} disabled={!selectedWorkspace || !workspaceReady}>
              <Terminal size={17} />
              TUI
            </button>
            <button className="refreshButton" onClick={forkChat} disabled={!selectedWorkspace || !workspaceReady || !activeChatState.session_id || chatBusy || !chatReady || !capabilityEnabled(sessionCapabilities, 'fork')}>
              <GitBranch size={17} />
              Fork
            </button>
            <button className="refreshButton" onClick={cancelChat} disabled={!selectedWorkspace || !workspaceReady || !activeChatState.session_id || !chatBusy}>
              Cancel
            </button>
          </div>
        </header>

        <div className="chatBody">
          {workspaceError ? (
            <div className="emptyChat">
              <p>Workspace error</p>
              <h2>{workspaceError}</h2>
            </div>
          ) : transcript.items.length === 0 && !pendingPermissions.length ? (
            <div className="emptyChat">
              <p>{chatReady ? 'Ready when you are.' : 'Starting Hermes ACP.'}</p>
              <h2>{acp.error || 'Message Hermes through Agent Mom.'}</h2>
            </div>
          ) : (
            <div className="messageList">
              {pendingPermissions.map((permission) => (
                <PermissionCard
                  key={permission.id}
                  permission={permission}
                  disabled={chatBusy}
                  onRespond={respondPermission}
                />
              ))}
              {transcript.items.map((message) => (
                <article key={message.key} className={`message ${message.role}`}>
                  <span>{message.title}</span>
                  <MessageContent message={message} />
                </article>
              ))}
            </div>
          )}
        </div>

        <form className="composer" onSubmit={sendMessage}>
          {promptAttachments.length > 0 && (
            <div className="attachmentTray">
              {promptAttachments.map((attachment, index) => (
                <span key={`${attachment.name}-${index}`}>
                  {attachment.name}
                  <button
                    type="button"
                    title="Remove image"
                    onClick={() =>
                      setPromptAttachments((current) => current.filter((_, itemIndex) => itemIndex !== index))
                    }
                  >
                    x
                  </button>
                </span>
              ))}
            </div>
          )}
          <input
            ref={fileInputRef}
            className="hiddenFileInput"
            type="file"
            accept="image/*"
            multiple
            onChange={attachPromptFiles}
          />
          <button
            type="button"
            disabled={!selectedWorkspace || !workspaceReady || busy || !promptCapabilities.image}
            title="Add image"
            onClick={() => fileInputRef.current?.click()}
          >
            <Plus size={20} />
          </button>
          <input
            value={chatInput}
            onChange={(event) => setChatInput(event.target.value)}
            placeholder={
              selectedWorkspace ? 'Message Hermes in this workspace' : 'Create a workspace first'
            }
            disabled={!selectedWorkspace || !workspaceReady || !activeChat || chatBusy || !chatReady}
          />
          <button className="sendButton" disabled={!selectedWorkspace || !workspaceReady || !activeChat || chatBusy || (!chatInput.trim() && !promptAttachments.length) || !chatReady}>
            {chatBusy ? <Sparkles size={20} /> : <Send size={20} />}
          </button>
        </form>
      </section>

      {previewOpen && (
        <PreviewPane
          previews={workspacePreviews}
          activePreview={activePreview}
          error={previewError}
          reloadKey={previewReloadKey}
          onSelect={setActivePreviewName}
          onReload={reloadPreview}
          onRemove={removeActivePreview}
          onClose={() => setPreviewOpen(false)}
        />
      )}

    </main>
  );
}

function PreviewPane({
  previews,
  activePreview,
  error,
  reloadKey,
  onSelect,
  onReload,
  onRemove,
  onClose,
}) {
  return (
    <section className="previewPane" aria-label="App preview">
      <header className="previewHeader">
        <div className="previewTabs" role="tablist" aria-label="Registered previews">
          {previews.map((preview) => (
            <button
              key={preview.name}
              type="button"
              className={preview.name === activePreview?.name ? 'active' : ''}
              onClick={() => onSelect(preview.name)}
            >
              {preview.name}
            </button>
          ))}
          {!previews.length && <span>No previews</span>}
        </div>
        <div className="previewActions">
          <button type="button" title="Reload preview" onClick={onReload} disabled={!activePreview}>
            <RefreshCcw size={16} />
          </button>
          <button
            type="button"
            title="Open preview"
            onClick={() => activePreview && window.open(activePreview.url, '_blank', 'noopener,noreferrer')}
            disabled={!activePreview}
          >
            <ExternalLink size={16} />
          </button>
          <button type="button" title="Remove preview" onClick={onRemove} disabled={!activePreview}>
            <Trash2 size={16} />
          </button>
          <button type="button" title="Close preview" onClick={onClose}>
            <X size={17} />
          </button>
        </div>
      </header>

      <div className="previewAddress">
        <code>{activePreview?.url ?? 'No preview URL'}</code>
      </div>

      <div className="previewFrameShell">
        {error ? (
          <div className="previewEmpty">
            <strong>Preview error</strong>
            <span>{error}</span>
          </div>
        ) : activePreview ? (
          <iframe
            key={`${activePreview.name}-${activePreview.url}-${reloadKey}`}
            title={`${activePreview.name} preview`}
            src={activePreview.url}
            sandbox="allow-downloads allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-same-origin allow-scripts"
          />
        ) : (
          <div className="previewEmpty">
            <strong>No registered app</strong>
            <span>Waiting for a workspace preview.</span>
          </div>
        )}
      </div>
    </section>
  );
}

function TuiPage({ userSession }) {
  const [workspaces, setWorkspaces] = useState([]);
  const [selectedName, setSelectedName] = useState(userSession.workspaceName ?? '');
  const [sessions, setSessions] = useState([]);
  const [activeSessionId, setActiveSessionId] = useState('');
  const [terminalKey, setTerminalKey] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  const selectedWorkspace = useMemo(
    () => workspaces.find((workspace) => workspace.name === selectedName) ?? workspaces[0] ?? null,
    [selectedName, workspaces],
  );

  useEffect(() => {
    refreshWorkspaces().catch((loadError) => setError(formatError(loadError)));
  }, []);

  useEffect(() => {
    if (!selectedWorkspace?.name) return;
    refreshSessions(selectedWorkspace.name).catch((loadError) => setError(formatError(loadError)));
  }, [selectedWorkspace?.name]);

  async function refreshWorkspaces() {
    setBusy(true);
    setError('');
    try {
      const allWorkspaces = normalizeWorkspaceList(await apiRequest('/workspaces'));
      const userWorkspace = selectUserWorkspace(allWorkspaces, userSession);
      const nextWorkspaces = userSession.workspaceName
        ? userWorkspace
          ? [userWorkspace]
          : []
        : allWorkspaces;
      setWorkspaces(nextWorkspaces);
      if (userWorkspace) {
        setSelectedName(userWorkspace.name);
      } else if (nextWorkspaces.length && !nextWorkspaces.some((workspace) => workspace.name === selectedName)) {
        setSelectedName(nextWorkspaces[0].name);
      }
      return nextWorkspaces;
    } finally {
      setBusy(false);
    }
  }

  async function refreshSessions(workspaceName = selectedWorkspace?.name) {
    if (!workspaceName) return [];
    setBusy(true);
    setError('');
    try {
      const data = await apiRequest(`/workspaces/${encodeURIComponent(workspaceName)}/tui/sessions`);
      const nextSessions = Array.isArray(data.sessions) ? data.sessions : [];
      setSessions(nextSessions);
      if (!activeSessionId && nextSessions[0]?.id) {
        setActiveSessionId(nextSessions[0].id);
      }
      return nextSessions;
    } finally {
      setBusy(false);
    }
  }

  function openSession(sessionId) {
    setActiveSessionId(sessionId);
    setTerminalKey((current) => current + 1);
  }

  function startSession() {
    setActiveSessionId('');
    setTerminalKey((current) => current + 1);
  }

  function backToChat() {
    window.location.href = '/';
  }

  return (
    <main className="tuiPage">
      <aside className="tuiSidebar">
        <button className="brandButton" type="button" onClick={backToChat}>
          <BuildersTableBrand />
        </button>
        <div className="tuiWorkspacePicker">
          <label htmlFor="tui-workspace">Workspace</label>
          <select
            id="tui-workspace"
            value={selectedWorkspace?.name ?? ''}
            onChange={(event) => {
              setSelectedName(event.target.value);
              setActiveSessionId('');
              setTerminalKey((current) => current + 1);
            }}
            disabled={busy || workspaces.length <= 1}
          >
            {workspaces.map((workspace) => (
              <option key={workspace.name} value={workspace.name}>
                {workspaceDisplayName(workspace)}
              </option>
            ))}
          </select>
        </div>

        <button className="launchButton" type="button" onClick={startSession} disabled={!selectedWorkspace}>
          <Terminal size={18} />
          New TUI
        </button>

        <div className="tuiSessionList">
          <div className="tuiSessionHeader">
            <h2>Sessions</h2>
            <button type="button" onClick={() => refreshSessions()} disabled={!selectedWorkspace || busy}>
              <RefreshCcw size={15} />
            </button>
          </div>
          {sessions.map((session) => (
            <button
              key={session.id}
              type="button"
              className={session.id === activeSessionId ? 'active' : ''}
              onClick={() => openSession(session.id)}
            >
              <MessageSquare size={15} />
              <span>{session.title || session.id}</span>
            </button>
          ))}
          {!sessions.length && <p>No saved TUI sessions.</p>}
        </div>
      </aside>

      <section className="tuiMain">
        <header className="tuiHeader">
          <div>
            <h1>{selectedWorkspace ? workspaceDisplayName(selectedWorkspace) : 'Hermes TUI'}</h1>
            <p>{selectedWorkspace ? friendlyStatus(selectedWorkspace.status) : 'No workspace selected'}</p>
          </div>
          <button className="refreshButton" type="button" onClick={backToChat}>
            <MessageSquare size={17} />
            Chat
          </button>
        </header>
        {error ? <div className="tuiError">{error}</div> : null}
        {selectedWorkspace ? (
          <TerminalPane
            key={`${selectedWorkspace.name}-${activeSessionId || 'new'}-${terminalKey}`}
            workspaceName={selectedWorkspace.name}
            sessionId={activeSessionId}
          />
        ) : (
          <div className="previewEmpty">
            <strong>No workspace</strong>
            <span>Create or select a workspace first.</span>
          </div>
        )}
      </section>
    </main>
  );
}

function TerminalPane({ workspaceName, sessionId }) {
  const containerRef = useRef(null);

  useEffect(() => {
    if (!containerRef.current) return undefined;
    let cancelled = false;
    let terminal = null;
    let socket = null;
    let dataDisposable = null;
    let resizeDisposable = null;
    let fit = null;

    async function startTerminal() {
      const [{ Terminal: LazyXTerm }, { FitAddon }] = await Promise.all([
        import('@xterm/xterm'),
        import('@xterm/addon-fit'),
        import('@xterm/xterm/css/xterm.css'),
      ]);
      if (cancelled || !containerRef.current) return;

      terminal = new LazyXTerm({
        cursorBlink: true,
        fontFamily: 'SFMono-Regular, Menlo, Monaco, Consolas, monospace',
        fontSize: 13,
        theme: {
          background: '#070809',
          foreground: '#f7f3ea',
          cursor: '#e39a4d',
        },
      });
      fit = new FitAddon();
      terminal.loadAddon(fit);
      terminal.open(containerRef.current);
      fit.fit();
      terminal.focus();
      terminal.writeln('Connecting to Hermes TUI...');

      socket = new WebSocket(tuiWsUrl(workspaceName, sessionId));
      socket.binaryType = 'arraybuffer';
      dataDisposable = terminal.onData((data) => {
        if (socket?.readyState === WebSocket.OPEN) {
          socket.send(data);
        }
      });
      resizeDisposable = terminal.onResize(({ cols, rows }) => {
        if (socket?.readyState === WebSocket.OPEN) {
          socket.send(`\x1b[RESIZE:${cols}x${rows}]`);
        }
      });

      socket.onopen = () => {
        terminal?.write('\r\n');
        fit?.fit();
      };
      socket.onmessage = (event) => {
        if (typeof event.data === 'string') {
          terminal?.write(event.data);
        } else if (event.data instanceof ArrayBuffer) {
          terminal?.write(new Uint8Array(event.data));
        } else if (event.data instanceof Blob) {
          event.data.arrayBuffer().then((buffer) => {
            if (!cancelled) {
              terminal?.write(new Uint8Array(buffer));
            }
          });
        }
      };
      socket.onerror = () => {
        terminal?.writeln('\r\nHermes TUI websocket failed.');
      };
      socket.onclose = () => {
        terminal?.writeln('\r\nHermes TUI disconnected.');
      };
    }

    startTerminal().catch((error) => {
      if (containerRef.current) {
        containerRef.current.textContent = `Hermes TUI failed to load: ${formatError(error)}`;
      }
    });

    const onResize = () => fit?.fit();
    window.addEventListener('resize', onResize);
    return () => {
      cancelled = true;
      window.removeEventListener('resize', onResize);
      dataDisposable?.dispose();
      resizeDisposable?.dispose();
      socket?.close();
      terminal?.dispose();
    };
  }, [workspaceName, sessionId]);

  return <div className="tuiTerminal" ref={containerRef} />;
}

function tuiWsUrl(workspaceName, sessionId) {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  const params = new URLSearchParams();
  if (sessionId) {
    params.set('resume', sessionId);
  }
  const query = params.toString();
  return `${protocol}//${window.location.host}${API_BASE}/workspaces/${encodeURIComponent(workspaceName)}/tui/pty${query ? `?${query}` : ''}`;
}

function MessageContent({ message }) {
  const blocks = (message.blocks ?? []).filter(Boolean);
  if (!blocks.length && message.text) {
    return <p>{message.text}</p>;
  }
  if (!blocks.length && message.raw) {
    return <pre>{JSON.stringify(message.raw, null, 2)}</pre>;
  }
  return (
    <div className="messageContent">
      {blocks.map((block, index) => (
        <MessageBlock block={block} key={`${block.type}-${index}`} />
      ))}
    </div>
  );
}

function MessageBlock({ block }) {
  if (block.type === 'text') {
    return <p>{block.text}</p>;
  }
  if (block.type === 'meta') {
    return (
      <dl className="metaBlock">
        {block.entries.map((entry) => (
          <React.Fragment key={entry.label}>
            <dt>{entry.label}</dt>
            <dd>{entry.value}</dd>
          </React.Fragment>
        ))}
      </dl>
    );
  }
  if (block.type === 'list') {
    return (
      <div className="listBlock">
        <strong>{block.title}</strong>
        <ul>
          {block.items.map((item, index) => (
            <li key={`${item}-${index}`}>{item}</li>
          ))}
        </ul>
      </div>
    );
  }
  if (block.type === 'plan') {
    return (
      <ol className="planBlock">
        {block.entries.map((entry, index) => (
          <li className={entry.status} key={`${entry.content}-${index}`}>
            <span>{entry.status}</span>
            <strong>{entry.content}</strong>
            {entry.priority && <small>{entry.priority}</small>}
          </li>
        ))}
      </ol>
    );
  }
  if (block.type === 'diff') {
    return (
      <div className="diffBlock">
        <strong>{block.path}</strong>
        <pre>{formatDiffBlock(block)}</pre>
      </div>
    );
  }
  if (block.type === 'image') {
    return (
      <figure className="mediaBlock">
        {block.src ? <img src={block.src} alt={block.label ?? 'ACP image'} /> : null}
        <figcaption>{block.label ?? block.mime ?? 'Image'}</figcaption>
      </figure>
    );
  }
  if (block.type === 'audio') {
    return (
      <div className="mediaBlock">
        {block.src ? <audio src={block.src} controls /> : null}
        <span>{block.label ?? block.mime ?? 'Audio'}</span>
      </div>
    );
  }
  if (block.type === 'resource') {
    return (
      <div className="resourceBlock">
        <strong>{block.title ?? block.uri ?? 'Resource'}</strong>
        {block.uri && <code>{block.uri}</code>}
        {[block.mime, block.size, block.description].filter(Boolean).length > 0 && (
          <span>{[block.mime, block.size, block.description].filter(Boolean).join(' / ')}</span>
        )}
      </div>
    );
  }
  if (block.type === 'terminal') {
    return <code className="terminalBlock">Terminal: {block.terminalId}</code>;
  }
  if (block.type === 'json') {
    return (
      <details className="jsonBlock" open>
        <summary>{block.title}</summary>
        <pre>{JSON.stringify(block.value, null, 2)}</pre>
      </details>
    );
  }
  return <pre>{JSON.stringify(block, null, 2)}</pre>;
}

function AdminPage({ userSession }) {
  const [users, setUsers] = useState([]);
  const [invites, setInvites] = useState([]);
  const [accessCodeStatus, setAccessCodeStatus] = useState('Generate code');
  const [adminError, setAdminError] = useState('');
  const [busy, setBusy] = useState(false);

  function leaveAdminView() {
    window.location.href = '/';
  }

  async function loadUsers() {
    setAdminError('');
    const data = await apiRequest('/admin/users');
    setUsers((data.users ?? []).map(normalizeAdminUser));
  }

  async function loadInvites() {
    const data = await apiRequest('/admin/invites');
    setInvites(data.invites ?? []);
  }

  useEffect(() => {
    loadUsers().catch((error) => setAdminError(formatError(error)));
    loadInvites().catch((error) => setAdminError(formatError(error)));
  }, [userSession?.id]);

  useEffect(() => {
    let cancelled = false;
    const checkAdminSession = async () => {
      try {
        const refreshedSession = await validateSession();
        if (cancelled) return;
        if (refreshedSession.role !== 'admin') {
          leaveAdminView();
        }
      } catch {
        if (!cancelled) {
          leaveAdminView();
        }
      }
    };

    const interval = window.setInterval(checkAdminSession, 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [userSession?.id]);

  async function refreshAccessCode() {
    setBusy(true);
    setAdminError('');
    try {
      const data = await apiRequest('/admin/invites', {
        method: 'POST',
        body: JSON.stringify({ label: `Invite ${new Date().toLocaleDateString()}` }),
      });
      setAccessCodeStatus(data.code);
      await loadInvites();
    } catch (error) {
      setAdminError(formatError(error));
    } finally {
      setBusy(false);
    }
  }

  async function deleteUser(user) {
    const previousUsers = users;
    setUsers((current) => current.filter((currentUser) => currentUser.id !== user.id));
    setAdminError('');

    try {
      await apiRequest(`/admin/users/${user.id}`, {
        method: 'DELETE',
      });
      if (sameEmail(user.email, userSession.email)) {
        leaveAdminView();
      }
    } catch (error) {
      setUsers(previousUsers);
      setAdminError(formatError(error));
    }
  }

  function openWorkspace() {
    window.location.href = '/';
  }

  return (
    <main className="adminPage">
      <section className="adminPanel" aria-label="Admin user management preview">
        <header className="adminTopBar">
          <div className="adminTopActions">
            <button className="adminNavButton" type="button" onClick={openWorkspace}>
              Workspace
            </button>
            <div className="accessCodeControl" aria-label="Access code">
              <code>{busy ? 'Generating...' : accessCodeStatus}</code>
              <button
                type="button"
                onClick={refreshAccessCode}
                title="Generate access code"
                disabled={busy}
              >
                <RefreshCcw size={17} />
              </button>
            </div>
          </div>
        </header>

        {adminError && <p className="adminError">{adminError}</p>}

        <section className="adminTableShell">
          <div className="adminTableHeader">
            <span>Invite</span>
            <span>Uses</span>
            <span>Status</span>
            <span>Code</span>
            <span></span>
          </div>
          <div className="adminUserList">
            {invites.map((invite) => (
              <article className="adminUserRow" key={invite.id}>
                <strong>{invite.label}</strong>
                <span>{invite.used_count}{invite.max_uses ? ` / ${invite.max_uses}` : ''}</span>
                <span>{invite.active ? 'active' : 'disabled'}</span>
                <span>{invite.code}</span>
                <span></span>
              </article>
            ))}
            {!invites.length && <p className="emptyList">No invites yet.</p>}
          </div>
        </section>

        <section className="adminTableShell">
          <div className="adminTableHeader">
            <span>Name</span>
            <span>Email</span>
            <span>Code</span>
            <span>Role</span>
            <span>Status</span>
            <span></span>
          </div>

          <div className="adminUserList">
            {users.map((user) => (
              <article className="adminUserRow" key={user.id}>
                <strong>{user.full_name || user.email}</strong>
                <span>{user.email}</span>
                <span>{user.code}</span>
                <span>{user.role}</span>
                <span
                  className={`adminStatusDot ${user.status}`}
                  title={adminStatusLabel(user.status)}
                  aria-label={adminStatusLabel(user.status)}
                />
                <button type="button" onClick={() => deleteUser(user)}>
                  Delete
                </button>
              </article>
            ))}
            {!users.length && <p className="emptyList">No users in the database yet.</p>}
          </div>
        </section>
      </section>
    </main>
  );
}

function friendlyStatus(status) {
  const lower = status.toLowerCase();
  if (lower === 'running' || lower === 'draining') return 'Ready';
  if (lower === 'stopped' || lower === 'idle-stopped') return 'Paused';
  if (lower === 'paused') return 'Paused';
  if (lower === 'crashed') return 'Needs attention';
  return status;
}

function isWorkspaceReady(workspace) {
  const status = String(workspace?.status ?? '').toLowerCase();
  return status === 'running' || status === 'draining';
}

function isWorkspaceStartable(workspace) {
  const status = String(workspace?.status ?? '').toLowerCase();
  return status === 'stopped' || status === 'idle-stopped' || status === 'paused';
}

function emptyAcpState() {
  return {
    state: 'starting',
    phase: 'idle',
    error: null,
    capabilities: {},
    agentInfo: null,
    authMethods: [],
  };
}

function emptyChatState() {
  return {
    events: [],
    session_id: null,
    creatingSession: false,
    loadingSession: false,
    loaded: false,
    models: null,
    modes: null,
    configOptions: [],
  };
}

function normalizeAcpSessions(workspaceName, result = {}) {
  return (result.sessions ?? [])
    .map((session) => {
      const sessionId = session.sessionId ?? session.session_id;
      if (!sessionId) return null;
      const updatedAt = parseAcpTimestamp(session.updatedAt ?? session.updated_at);
      return {
        id: sessionId,
        sessionId,
        acpSessionId: sessionId,
        temporary: false,
        workspaceName,
        title: session.title || 'Untitled chat',
        createdAt: updatedAt,
        updatedAt,
      };
    })
    .filter(Boolean);
}

function sessionTitle(result = {}) {
  return result.title ?? result.session?.title ?? '';
}

function parseAcpTimestamp(value) {
  if (!value) return Date.now();
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) ? timestamp : Date.now();
}

function newChatId(workspaceName) {
  return `tmp:${workspaceName}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function chatWsUrl(workspaceName) {
  const base = new URL(API_BASE, window.location.href);
  const protocol = base.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${base.host}${base.pathname.replace(/\/$/, '')}/workspaces/${encodeURIComponent(workspaceName)}/chat/ws`;
}

function parseJsonMessage(data) {
  if (typeof data !== 'string') {
    return { jsonrpc: '2.0', method: 'mom/binary', params: { bytes: data?.byteLength ?? 0 } };
  }
  try {
    return JSON.parse(data);
  } catch {
    return { jsonrpc: '2.0', method: 'mom/text', params: { text: data } };
  }
}

function rpcKey(workspaceName, id) {
  return `${workspaceName}:${String(id)}`;
}

function extractSessionId(message) {
  return (
    message?.params?.sessionId ??
    message?.params?.session_id ??
    message?.result?.sessionId ??
    message?.result?.session_id ??
    null
  );
}

function titleFromSessionUpdate(message) {
  if (message?.method !== 'session/update') return '';
  const update = message.params?.update ?? {};
  const type = update.sessionUpdate ?? update.session_update ?? update.type;
  if (type !== 'session_info_update') return '';
  return String(update.title ?? '').trim();
}

function parseJsonRpcId(id) {
  const text = String(id);
  return /^\d+$/.test(text) ? Number(text) : text;
}

function permissionResult(optionId) {
  const denied = ['deny', 'deny_always', 'reject', 'cancel', 'cancelled', 'canceled'].includes(optionId);
  if (denied) {
    return { outcome: { outcome: 'cancelled' } };
  }
  return {
    outcome: {
      outcome: 'selected',
      optionId,
      option_id: optionId,
    },
  };
}

function capabilityEnabled(capabilities, key) {
  const value = capabilities?.[key] ?? capabilities?.[`can_${key}`] ?? capabilities?.[`can${key[0]?.toUpperCase() ?? ''}${key.slice(1)}`];
  return value !== false;
}

function availableModels(models = {}) {
  return (models.availableModels ?? models.available_models ?? [])
    .map((model) => ({
      modelId: model.modelId ?? model.model_id,
      name: model.name ?? model.modelId ?? model.model_id,
    }))
    .filter((model) => model.modelId);
}

function availableModes(modes = {}) {
  return (modes.availableModes ?? modes.available_modes ?? [])
    .map((mode) => ({
      modeId: mode.id ?? mode.modeId ?? mode.mode_id,
      name: mode.name ?? mode.id ?? mode.modeId ?? mode.mode_id,
    }))
    .filter((mode) => mode.modeId);
}

function patchCurrentModel(models, modelId) {
  if (!models) return models;
  return {
    ...models,
    currentModelId: modelId,
    current_model_id: modelId,
  };
}

function patchCurrentMode(modes, modeId) {
  if (!modes) return modes;
  return {
    ...modes,
    currentModeId: modeId,
    current_mode_id: modeId,
  };
}

function patchConfigOption(options = [], optionId, value) {
  return options.map((option) =>
    option.id === optionId
      ? {
          ...option,
          currentValue: value,
          current_value: value,
        }
      : option,
  );
}

function configSelectOptions(option) {
  return (option.options ?? []).flatMap((choice) => {
    if (Array.isArray(choice.options)) return configSelectOptions(choice);
    const value = choice.value ?? choice.id;
    return value == null
      ? []
      : [{
          value: String(value),
          label: choice.name ?? choice.label ?? String(value),
        }];
  });
}

function promptBlocks(text, attachments) {
  return [
    text ? { type: 'text', text } : null,
    ...attachments.map((attachment) => ({
      type: 'image',
      data: attachment.data,
      mimeType: attachment.mimeType,
      uri: attachment.name,
    })),
  ].filter(Boolean);
}

async function readImageAttachment(file) {
  if (!file.type.startsWith('image/')) {
    throw new Error(`${file.name} is not an image.`);
  }
  const dataUrl = await readFileDataUrl(file);
  const [, data = ''] = dataUrl.split(',', 2);
  return {
    name: file.name,
    mimeType: file.type || 'image/png',
    data,
  };
}

function readFileDataUrl(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result ?? ''));
    reader.onerror = () => reject(reader.error ?? new Error(`Could not read ${file.name}`));
    reader.readAsDataURL(file);
  });
}

function PermissionCard({ permission, disabled, onRespond }) {
  const params = permission.params ?? {};
  const options = params.options ?? params.permissionOptions ?? params.permission_options ?? [];
  const fallbackOptions = options.length
    ? options
    : [
        { id: 'allow_once', name: 'Allow once' },
        { id: 'deny', name: 'Deny' },
      ];
  const tool = params.toolCall ?? params.tool_call ?? params;
  const title = tool.title ?? tool.name ?? permission.method ?? 'Permission requested';

  return (
    <article className="permissionCard">
      <span>Hermes needs permission</span>
      <strong>{title}</strong>
      <pre>{JSON.stringify(tool, null, 2)}</pre>
      <div>
        {fallbackOptions.map((option) => {
          const id = option.id ?? option.optionId ?? option.option_id ?? option.name;
          return (
            <button
              key={id}
              type="button"
              disabled={disabled || !id}
              onClick={() => onRespond(permission, id)}
            >
              {option.name ?? option.label ?? id}
            </button>
          );
        })}
      </div>
    </article>
  );
}

async function createWorkspaceFromOnboarding(userName, agentName) {
  const response = await apiRequest('/me/setup', {
    method: 'POST',
    body: JSON.stringify({
      full_name: userName,
      agent_name: agentName,
    }),
  });
  return sessionFromMe(response);
}

async function apiRequest(path, options = {}) {
  const headers = {
    'Content-Type': 'application/json',
    ...(options.headers ?? {}),
  };
  const response = await fetch(`${API_BASE}${path}`, {
    ...options,
    headers,
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw data;
  }
  return data;
}

function workspaceAlreadyExists(error) {
  const message = formatError(error).toLowerCase();
  return message.includes('already exists') || message.includes('replace');
}

function sessionFromMe(data) {
  const user = data.user ?? {};
  const workspace = data.workspace ?? null;
  return {
    id: user.id,
    email: user.email,
    code: user.code,
    role: user.role,
    userName: user.full_name ?? '',
    agentName: workspace?.agent_name ?? workspace?.agentName ?? '',
    workspaceName: workspace?.name ?? '',
    workspaceDisplayName: workspace ? workspaceDisplayName(workspace) : '',
  };
}

function workspaceDisplayName(workspace) {
  return workspace.display_name ?? workspace.displayName ?? workspace.name ?? 'Agent workspace';
}

function normalizeWorkspaceList(data) {
  if (Array.isArray(data)) return data;
  if (Array.isArray(data?.vms)) return data.vms;
  return [];
}

function selectUserWorkspace(workspaces, session) {
  const savedName = session.workspaceName;
  if (savedName) {
    return workspaces.find((workspace) => workspace.name === savedName) ?? null;
  }
  return findWorkspaceBySubmittedName(workspaces, session.agentName);
}

function findWorkspaceBySubmittedName(workspaces, submittedName) {
  const normalized = String(submittedName ?? '').trim().toLowerCase();
  if (!normalized) return null;
  return (
    workspaces.find((workspace) =>
      [workspace.name, workspace.slug, workspace.display_name, workspace.displayName]
        .filter(Boolean)
        .some((value) => String(value).trim().toLowerCase() === normalized),
    ) ?? null
  );
}

function launchUrlFromResult(result) {
  const rawUrl = result.stdout?.trim().split(/\s+/).at(-1);
  if (!rawUrl) return '';

  try {
    const url = new URL(rawUrl, window.location.href);
    if (url.hostname === 'agentmom.xyz' && url.pathname.startsWith('/tunnels/')) {
      return `${url.pathname}${url.search}${url.hash}`;
    }
    return url.href;
  } catch {
    return rawUrl;
  }
}

function adminStatusLabel(status) {
  if (status === 'active') return 'Active';
  if (status === 'idle') return 'Idle';
  if (status === 'stagnant') return 'Stagnant';
  return 'Inactive';
}

function normalizeAdminUser(user) {
  return {
    ...user,
    id: String(user.id),
    status: userDisplayStatus(user),
  };
}

function userDisplayStatus(user) {
  if (user.status === 'inactive' || !user.last_seen_at) return 'inactive';
  const inactiveAfter = 15 * 60;
  const stagnantAfter = 5 * 60;
  const ageSeconds = Math.max(0, Math.floor(Date.now() / 1000) - user.last_seen_at);
  if (ageSeconds >= inactiveAfter) return 'inactive';
  if (ageSeconds >= stagnantAfter) return 'stagnant';
  return 'active';
}

function chatTitle(prompt) {
  const title = prompt.trim().replace(/\s+/g, ' ');
  return title.length > 34 ? `${title.slice(0, 34)}...` : title || 'New chat';
}

function groupChatsByAge(chats, now) {
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);

  const startOfThisWeek = new Date(startOfToday);
  startOfThisWeek.setDate(startOfThisWeek.getDate() - 6);

  const groups = [
    { label: 'Today', chats: [] },
    { label: 'This week', chats: [] },
    { label: 'Older', chats: [] },
  ];

  chats.forEach((chat) => {
    const timestamp = chat.updatedAt ?? chat.createdAt ?? 0;
    if (timestamp >= startOfToday.getTime()) {
      groups[0].chats.push(chat);
    } else if (timestamp >= startOfThisWeek.getTime()) {
      groups[1].chats.push(chat);
    } else {
      groups[2].chats.push(chat);
    }
  });

  return groups.filter((group) => group.chats.length);
}

function formatDiffBlock(block) {
  const lines = [];
  if (block.oldText) {
    lines.push(...String(block.oldText).split('\n').map((line) => `- ${line}`));
  }
  if (block.newText) {
    lines.push(...String(block.newText).split('\n').map((line) => `+ ${line}`));
  }
  return lines.join('\n') || 'No diff content.';
}

function userIdentity(email) {
  return String(email ?? '').trim().toLowerCase();
}

function sameEmail(left, right) {
  return userIdentity(left) === userIdentity(right);
}

function renderResult(result) {
  const output = [result.stdout, result.stderr].filter(Boolean).join('\n');
  return output || `Done.`;
}

function formatError(error) {
  if (error?.stdout || error?.stderr) {
    return renderResult(error);
  }
  if (error?.error === 'no ready worker nodes are registered') {
    return 'No worker is running, so Agent Mom cannot create a workspace yet.';
  }
  return error?.error ?? String(error);
}

createRoot(document.getElementById('root')).render(<Root />);
