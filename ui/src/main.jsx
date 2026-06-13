import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  Edit3,
  ExternalLink,
  PanelLeft,
  Plus,
  RefreshCcw,
  Send,
  Sparkles,
  Users,
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
  const [acpByWorkspace, setAcpByWorkspace] = useState({});
  const [chatRestartNonce, setChatRestartNonce] = useState(0);
  const [workspaceError, setWorkspaceError] = useState('');
  const [now, setNow] = useState(() => Date.now());
  const chatSocketsRef = useRef({});
  const pendingRpcRef = useRef({});
  const rpcIdRef = useRef(1);

  const selectedWorkspace = useMemo(
    () => workspaces.find((workspace) => workspace.name === selectedName) ?? workspaces[0],
    [selectedName, workspaces],
  );
  const selectedKey = selectedWorkspace?.name ?? selectedName;
  const acp = selectedKey ? acpByWorkspace[selectedKey] ?? emptyAcpState() : emptyAcpState();
  const transcript = useMemo(() => buildTranscript(acp.events), [acp.events]);
  const pendingPermissions = useMemo(() => buildPendingPermissions(acp.events), [acp.events]);
  const chatGroups = groupChatsByAge(
    selectedWorkspace
      ? [
          {
            id: selectedWorkspace.name,
            title: workspaceDisplayName(selectedWorkspace),
            createdAt: acp.startedAt ?? now,
            updatedAt: acp.updatedAt ?? now,
          },
        ]
      : [],
    now,
  );

  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(interval);
  }, []);

  useEffect(() => {
    refresh().catch((error) => setWorkspaceError(formatError(error)));
  }, []);

  useEffect(() => {
    if (!selectedWorkspace?.name) return undefined;

    const workspaceName = selectedWorkspace.name;
    const socket = new WebSocket(chatWsUrl(workspaceName));
    let cancelled = false;

    chatSocketsRef.current[workspaceName]?.close();
    chatSocketsRef.current[workspaceName] = socket;
    setAcpConnectionState(workspaceName, {
      state: 'connecting',
      phase: 'websocket',
      error: null,
      events: [],
      session_id: null,
    });

    socket.onopen = () => {
      if (cancelled) return;
      setAcpConnectionState(workspaceName, { state: 'open', phase: 'initialize', error: null });
      sendRpc(workspaceName, 'initialize', {
        protocolVersion: 1,
        clientCapabilities: {},
        clientInfo: { name: 'agent-mom', version: '0.1.1' },
      });
    };

    socket.onmessage = (event) => {
      if (cancelled) return;
      const message = parseJsonMessage(event.data);
      appendAcpEvent(workspaceName, 'in', message);

      if (message.method === 'mom/status') {
        const params = message.params ?? {};
        setAcpConnectionState(workspaceName, {
          state: params.state === 'error' ? 'failed' : params.state ?? 'open',
          phase: params.state === 'ready' ? 'initialize' : params.state,
          error: params.message ?? null,
        });
        return;
      }

      if (message.id != null) {
        const key = rpcKey(workspaceName, message.id);
        const method = pendingRpcRef.current[key];
        delete pendingRpcRef.current[key];
        if (message.error) {
          setAcpConnectionState(workspaceName, {
            state: 'failed',
            phase: method ?? 'rpc',
            error: JSON.stringify(message.error),
          });
          return;
        }
        if (method === 'initialize') {
          setAcpConnectionState(workspaceName, { state: 'open', phase: 'session/new' });
          sendRpc(workspaceName, 'session/new', { cwd: '/workspace', mcpServers: [] });
        } else if (method === 'session/new') {
          const sessionId = message.result?.sessionId ?? message.result?.session_id;
          setAcpConnectionState(workspaceName, {
            state: sessionId ? 'ready' : 'open',
            phase: 'ready',
            session_id: sessionId ?? null,
          });
        }
      }
    };

    socket.onerror = () => {
      if (cancelled) return;
      setAcpConnectionState(workspaceName, {
        state: 'failed',
        phase: 'websocket',
        error: 'Hermes ACP websocket failed.',
      });
    };

    socket.onclose = () => {
      if (cancelled) return;
      setAcpConnectionState(workspaceName, { state: 'exited', phase: 'closed' });
    };

    return () => {
      cancelled = true;
      if (chatSocketsRef.current[workspaceName] === socket) {
        delete chatSocketsRef.current[workspaceName];
      }
      socket.close();
    };
  }, [selectedWorkspace?.name, chatRestartNonce]);

  async function request(path, options = {}) {
    setBusy(true);
    try {
      return await apiRequest(path, options, userSession);
    } finally {
      setBusy(false);
    }
  }

  function appendAcpEvent(workspaceName, direction, message) {
    setAcpByWorkspace((current) => {
      const previous = current[workspaceName] ?? emptyAcpState();
      const seq = (previous.events.at(-1)?.seq ?? 0) + 1;
      return {
        ...current,
        [workspaceName]: {
          ...previous,
          events: [...previous.events, { seq, at: Date.now(), direction, message }],
          startedAt: previous.startedAt ?? Date.now(),
          updatedAt: Date.now(),
        },
      };
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

  function sendRpc(workspaceName, method, params = {}) {
    const id = rpcIdRef.current++;
    const message = { jsonrpc: '2.0', id, method, params };
    pendingRpcRef.current[rpcKey(workspaceName, id)] = method;
    sendAcpMessage(workspaceName, message);
    return id;
  }

  function sendAcpMessage(workspaceName, message) {
    const socket = chatSocketsRef.current[workspaceName];
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new Error('Hermes ACP websocket is not connected.');
    }
    socket.send(JSON.stringify(message));
    appendAcpEvent(workspaceName, 'out', message);
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

  function openAdminPage() {
    window.location.href = '/admin';
  }

  async function sendMessage(event) {
    event.preventDefault();
    if (!selectedWorkspace) return;

    const prompt = chatInput.trim();
    if (!prompt) return;

    setChatInput('');
    setChatBusy(true);
    try {
      sendRpc(selectedWorkspace.name, 'session/prompt', {
        sessionId: acp.session_id,
        messageId: `agent-mom-${Date.now()}`,
        prompt: [{ type: 'text', text: prompt }],
      });
      await refresh();
    } catch (error) {
      setAcpConnectionState(selectedWorkspace.name, {
        state: 'failed',
        phase: 'send',
        error: formatError(error),
      });
    } finally {
      setChatBusy(false);
    }
  }

  async function restartChat() {
    if (!selectedWorkspace) return;
    setChatBusy(true);
    try {
      chatSocketsRef.current[selectedWorkspace.name]?.close();
      setChatRestartNonce((value) => value + 1);
    } catch (error) {
      setWorkspaceError(formatError(error));
    } finally {
      setChatBusy(false);
    }
  }

  async function cancelChat() {
    if (!selectedWorkspace) return;
    setChatBusy(true);
    try {
      sendAcpMessage(selectedWorkspace.name, {
        jsonrpc: '2.0',
        method: 'session/cancel',
        params: { sessionId: acp.session_id },
      });
    } catch (error) {
      setWorkspaceError(formatError(error));
    } finally {
      setChatBusy(false);
    }
  }

  async function respondPermission(permission, optionId) {
    if (!selectedWorkspace) return;
    setChatBusy(true);
    try {
      sendAcpMessage(selectedWorkspace.name, {
        jsonrpc: '2.0',
        id: parseJsonRpcId(permission.id),
        result: permissionResult(optionId),
      });
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
    if (!selectedWorkspace) {
      setWorkspaceError(content);
      return;
    }
    setAcpConnectionState(selectedWorkspace.name, {
      state: acp.state,
      phase: acp.phase,
      error: content,
    });
  }

  return (
    <main className="appShell">
      <aside className="sidebar">
        <div className="userDropdown">
          <div className="brandRow">
            <div className="brandMark">A</div>
            <div className="brandButton">Agent Mom</div>
          </div>
        </div>

        <div className="sidebarQuickActions">
          <button className="launchButton" onClick={restartChat} disabled={!selectedWorkspace || chatBusy}>
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
                  <button
                    key={chat.id}
                    className="chatHistoryItem active"
                    onClick={() => setSelectedName(chat.id)}
                  >
                    <span>{chat.title}</span>
                  </button>
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
            <p>{selectedWorkspace ? friendlyStatus(selectedWorkspace.status) : 'Create a workspace to begin.'}</p>
            {selectedWorkspace && (
              <small className={`acpStatus ${acp.state}`}>
                Hermes ACP: {acp.state}
                {acp.phase && ` (${acp.phase})`}
              </small>
            )}
          </div>
          <div className="headerActions">
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
            <button className="refreshButton" onClick={launchHermes} disabled={!selectedWorkspace || busy}>
              <ExternalLink size={17} />
              Hermes
            </button>
            <button className="refreshButton" onClick={cancelChat} disabled={!selectedWorkspace || chatBusy || acp.state !== 'ready'}>
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
              <p>{acp.state === 'ready' ? 'Ready when you are.' : 'Starting Hermes ACP.'}</p>
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
                  {message.text && <p>{message.text}</p>}
                  {!message.text && message.raw && <pre>{JSON.stringify(message.raw, null, 2)}</pre>}
                </article>
              ))}
            </div>
          )}
        </div>

        <form className="composer" onSubmit={sendMessage}>
          <button type="button" disabled={!selectedWorkspace || busy} title="Add context">
            <Plus size={20} />
          </button>
          <input
            value={chatInput}
            onChange={(event) => setChatInput(event.target.value)}
            placeholder={
              selectedWorkspace ? 'Message Hermes in this workspace' : 'Create a workspace first'
            }
            disabled={!selectedWorkspace || chatBusy || acp.state !== 'ready'}
          />
          <button className="sendButton" disabled={!selectedWorkspace || chatBusy || !chatInput.trim() || acp.state !== 'ready'}>
            {chatBusy ? <Sparkles size={20} /> : <Send size={20} />}
          </button>
        </form>
      </section>

    </main>
  );
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
  if (lower === 'stopped') return 'Paused';
  if (lower === 'paused') return 'Paused';
  if (lower === 'crashed') return 'Needs attention';
  return status;
}

function emptyAcpState() {
  return {
    events: [],
    session_id: null,
    state: 'starting',
    phase: 'idle',
    error: null,
  };
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
