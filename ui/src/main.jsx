import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  ChevronDown,
  Edit3,
  PanelLeft,
  Plus,
  RefreshCcw,
  Send,
  Sparkles,
  UserCircle,
} from 'lucide-react';
import './styles.css';

const API_BASE = import.meta.env.VITE_API_BASE ?? '/api';
const CHAT_STORAGE_KEY = 'agent-mom-chats';
const USER_SESSION_KEY = 'agent-mom-user-session';
const ADMIN_EMAIL = 'autumndomingo@gmail.com';

function Root() {
  const [userSession, setUserSession] = useState(null);
  const [checkingSession, setCheckingSession] = useState(true);

  useEffect(() => {
    const stored = loadUserSession();
    if (!stored) {
      setCheckingSession(false);
      return;
    }

    validateSession(stored)
      .then((session) => {
        setUserSession(session);
      })
      .catch(() => {
        window.localStorage.removeItem(USER_SESSION_KEY);
      })
      .finally(() => {
        setCheckingSession(false);
      });
  }, []);

  function enterUserFlow(session) {
    window.localStorage.setItem(USER_SESSION_KEY, JSON.stringify(session));
    setUserSession(session);
  }

  if (window.location.pathname === '/admin') {
    return <AdminPage />;
  }

  if (checkingSession) {
    return (
      <main className="landingPage">
        <section className="landingPanel" aria-label="User access">
          <div className="landingHeader">
            <div className="brandMark">A</div>
            <h1>Agent Mom</h1>
          </div>
        </section>
      </main>
    );
  }

  if (!userSession) {
    return <LandingPage onSubmit={enterUserFlow} />;
  }

  if (!userSession.userName || !userSession.agentName) {
    return <SetupPage userSession={userSession} onSubmit={enterUserFlow} />;
  }

  return <App userSession={userSession} />;
}

function LandingPage({ onSubmit }) {
  const [form, setForm] = useState({ email: ADMIN_EMAIL, accessCode: '' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  async function submitAccess(event) {
    event.preventDefault();
    const email = form.email.trim();
    const accessCode = form.accessCode.trim();
    if (!email || !accessCode) return;

    setBusy(true);
    setError('');
    try {
      const response = await fetch(`${API_BASE}/auth/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email, access_code: accessCode }),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      onSubmit({
        email: data.email,
        role: data.role,
        token: data.token,
        startedAt: Date.now(),
      });
    } catch (accessError) {
      setError(accessError?.error ?? 'Access denied.');
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="landingPage">
      <section className="landingPanel" aria-label="User access">
        <div className="landingHeader">
          <div className="brandMark">A</div>
          <h1>Agent Mom</h1>
        </div>

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
            required
          />
          {error && <p className="accessError">{error}</p>}
          <button disabled={busy || !form.email.trim() || !form.accessCode.trim()}>
            {busy ? 'Checking...' : 'Continue'}
          </button>
        </form>
      </section>
    </main>
  );
}

async function validateSession(session) {
  if (!session.token) {
    throw new Error('Missing session token.');
  }
  const response = await fetch(`${API_BASE}/auth/session`, {
    headers: authHeaders(session),
  });
  const data = await response.json();
  if (!response.ok) {
    throw data;
  }
  return {
    ...session,
    email: data.email,
    role: data.role,
  };
}

function SetupPage({ userSession, onSubmit }) {
  const [form, setForm] = useState({
    userName: userSession.userName ?? '',
    agentName: userSession.agentName ?? '',
  });

  function submitSetup(event) {
    event.preventDefault();
    const userName = form.userName.trim();
    const agentName = form.agentName.trim();
    if (!userName || !agentName) return;

    onSubmit({
      ...userSession,
      userName,
      agentName,
      completedSetupAt: Date.now(),
    });
  }

  return (
    <main className="landingPage">
      <section className="landingPanel setupPanel" aria-label="Create your workspace">
        <div className="landingHeader">
          <div className="brandMark">A</div>
          <h1>Create workspace</h1>
        </div>

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
          <button disabled={!form.userName.trim() || !form.agentName.trim()}>Continue</button>
        </form>
      </section>
    </main>
  );
}

function App({ userSession }) {
  const [vms, setVms] = useState(() => [
    { name: userSession.agentName, userName: userSession.userName, status: 'paused' },
  ]);
  const [selectedName, setSelectedName] = useState(userSession.agentName);
  const [busy, setBusy] = useState(false);
  const [showUsers, setShowUsers] = useState(false);
  const [showCreate, setShowCreate] = useState(false);
  const [createForm, setCreateForm] = useState({ userName: userSession.userName, botName: '' });
  const [chatInput, setChatInput] = useState('');
  const [chatsByUser, setChatsByUser] = useState(() => loadStoredChats());
  const [activeChatByUser, setActiveChatByUser] = useState({});
  const [activityByName, setActivityByName] = useState({});
  const [now, setNow] = useState(() => Date.now());

  const selectedVm = useMemo(
    () => vms.find((vm) => vm.name === selectedName) ?? vms[0],
    [selectedName, vms],
  );
  const selectedChats = selectedVm ? chatsByUser[selectedVm.name] ?? [] : [];
  const activeChatId = selectedVm ? activeChatByUser[selectedVm.name] ?? selectedChats[0]?.id : undefined;
  const activeChat = selectedChats.find((chat) => chat.id === activeChatId);
  const chatGroups = groupChatsByAge(selectedChats, now);
  const messages = activeChat?.messages ?? [];

  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(interval);
  }, []);

  useEffect(() => {
    window.localStorage.setItem(CHAT_STORAGE_KEY, JSON.stringify(chatsByUser));
  }, [chatsByUser]);

  async function refresh() {
    setVms((current) =>
      current.map((vm) => (vm.name === selectedName ? { ...vm, status: 'paused' } : vm)),
    );
  }

  async function createWorkspace(event) {
    event.preventDefault();
    const name = createForm.botName.trim();
    if (!name) return;

    setVms((current) => {
      const withoutDuplicate = current.filter((vm) => vm.name !== name);
      return [{ name, userName: createForm.userName.trim() || userSession.userName, status: 'paused' }, ...withoutDuplicate];
    });
    setCreateForm({ userName: userSession.userName, botName: '' });
    setShowCreate(false);
    setSelectedName(name);
    markActive(name);
    startNewChat(name);
  }

  async function sendMessage(event) {
    event.preventDefault();
    if (!selectedVm) return;

    const prompt = chatInput.trim();
    if (!prompt) return;

    setChatInput('');
    markActive(selectedVm.name);
    const chatId = ensureChatForPrompt(selectedVm.name, prompt);
    appendMessage(selectedVm.name, chatId, { role: 'user', content: prompt });

    appendMessage(selectedVm.name, chatId, {
      role: 'assistant',
      content: 'This prototype is connected through the local onboarding flow. Backend chat wiring can be added after the screen flow is finalized.',
    });
  }

  function selectWorkspace(name) {
    setSelectedName(name);
    setShowUsers(false);
    markActive(name);
  }

  function markActive(name) {
    setActivityByName((current) => ({ ...current, [name]: Date.now() }));
  }

  function startNewChat(name = selectedVm?.name) {
    if (!name) return;
    const chat = {
      id: window.crypto?.randomUUID?.() ?? `${Date.now()}`,
      title: 'New chat',
      createdAt: Date.now(),
      updatedAt: Date.now(),
      messages: [],
    };
    setChatsByUser((current) => ({
      ...current,
      [name]: [chat, ...(current[name] ?? [])],
    }));
    setActiveChatByUser((current) => ({ ...current, [name]: chat.id }));
  }

  function selectChat(chatId) {
    if (!selectedVm) return;
    setActiveChatByUser((current) => ({ ...current, [selectedVm.name]: chatId }));
  }

  function ensureChatForPrompt(name, prompt) {
    const existing = activeChatByUser[name] ?? chatsByUser[name]?.[0]?.id;
    if (existing) return existing;

    const chat = {
      id: window.crypto?.randomUUID?.() ?? `${Date.now()}`,
      title: chatTitle(prompt),
      createdAt: Date.now(),
      updatedAt: Date.now(),
      messages: [],
    };
    setChatsByUser((current) => ({
      ...current,
      [name]: [chat, ...(current[name] ?? [])],
    }));
    setActiveChatByUser((current) => ({ ...current, [name]: chat.id }));
    return chat.id;
  }

  function appendMessage(name, chatId, message) {
    setChatsByUser((current) => ({
      ...current,
      [name]: (current[name] ?? []).map((chat) =>
        chat.id === chatId
          ? {
              ...chat,
              title:
                chat.title === 'New chat' && message.role === 'user'
                  ? chatTitle(message.content)
                  : chat.title,
              updatedAt: Date.now(),
              messages: [...chat.messages, message],
            }
          : chat,
      ),
    }));
  }

  function openAdminProfile() {
    window.location.href = '/admin';
  }

  return (
    <main className="appShell">
      <aside className="sidebar">
        <div className="userDropdown">
          <div className="brandRow">
            <div className="brandMark">A</div>
            <button
              className="brandButton"
              onClick={() => setShowUsers((value) => !value)}
              aria-expanded={showUsers}
            >
              Agent Mom Users
              <ChevronDown className={showUsers ? 'chevron open' : 'chevron'} size={16} />
            </button>
          </div>

          {showUsers && (
            <div className="userMenu">
              {vms.map((vm) => {
                const status = userStatus(vm, activityByName[vm.name], now);
                return (
                  <button
                    key={vm.name}
                    className={`userMenuItem ${selectedVm?.name === vm.name ? 'active' : ''}`}
                    onClick={() => selectWorkspace(vm.name)}
                  >
                    <span className={`statusDot ${status}`} />
                    <span>{vm.userName ?? userSession.userName}</span>
                    <small>{statusLabel(status)}</small>
                  </button>
                );
              })}
              {!vms.length && <p className="emptyList">No users yet.</p>}
            </div>
          )}
        </div>

        <div className="sidebarQuickActions">
          <button className="newChatButton" onClick={() => setShowCreate(true)}>
            <Plus size={24} strokeWidth={2.25} />
            Create
          </button>

          <button className="launchButton" onClick={() => startNewChat()} disabled={!selectedVm}>
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
                    className={`chatHistoryItem ${chat.id === activeChatId ? 'active' : ''}`}
                    onClick={() => selectChat(chat.id)}
                  >
                    <span>{chat.title}</span>
                  </button>
                ))}
              </div>
            </section>
          ))}
          {!selectedVm && <p className="emptyList">Create a user to start chatting.</p>}
          {selectedVm && !selectedChats.length && <p className="emptyList">New chats will appear here.</p>}
        </div>

        <div className="sessionBox">
          <h2>Session</h2>
          <strong>Local workspace</strong>
          <span>{userSession.email}</span>
        </div>
      </aside>

      <section className="chatShell">
        <header className="chatHeader">
          <button className="squareButton" title="Toggle sidebar">
            <PanelLeft size={20} />
          </button>
          <div>
            <h1>{selectedVm?.name ?? 'Agent workspace'}</h1>
            <p>{selectedVm ? friendlyStatus(selectedVm.status) : 'Create a workspace to begin.'}</p>
          </div>
          <div className="headerActions">
            <button className="refreshButton" onClick={openAdminProfile}>
              <UserCircle size={17} />
              Profile
            </button>
            <button className="refreshButton" onClick={refresh} disabled={busy}>
              <RefreshCcw size={17} />
              Refresh
            </button>
          </div>
        </header>

        <div className="chatBody">
          {messages.length === 0 ? (
            <div className="emptyChat">
              <p>Ready when you are.</p>
              <h2>Ask Agent Mom about your workspace.</h2>
            </div>
          ) : (
            <div className="messageList">
              {messages.map((message, index) => (
                <article key={`${message.role}-${index}`} className={`message ${message.role}`}>
                  <span>{message.role === 'user' ? 'You' : 'Agent Mom'}</span>
                  <p>{message.content}</p>
                </article>
              ))}
            </div>
          )}
        </div>

        <form className="composer" onSubmit={sendMessage}>
          <button type="button" disabled={!selectedVm || busy} title="Add context">
            <Plus size={20} />
          </button>
          <input
            value={chatInput}
            onChange={(event) => setChatInput(event.target.value)}
            placeholder={
              selectedVm ? 'Ask Agent Mom anything about this workspace' : 'Create a workspace first'
            }
            disabled={!selectedVm || busy}
          />
          <button className="sendButton" disabled={!selectedVm || busy || !chatInput.trim()}>
            {busy ? <Sparkles size={20} /> : <Send size={20} />}
          </button>
        </form>
      </section>

      {showCreate && (
        <div className="modalBackdrop" role="presentation" onMouseDown={() => setShowCreate(false)}>
          <form
            className="createModal"
            onSubmit={createWorkspace}
            onMouseDown={(event) => event.stopPropagation()}
          >
            <div>
              <h2>Create new user</h2>
              <p>Name yourself and the agent you want to chat with.</p>
            </div>
            <label>
              <span>Name</span>
              <input
                value={createForm.userName}
                onChange={(event) =>
                  setCreateForm((current) => ({ ...current, userName: event.target.value }))
                }
                placeholder="Name"
                autoFocus
              />
            </label>
            <label>
              <span>Agent name</span>
              <input
                value={createForm.botName}
                onChange={(event) =>
                  setCreateForm((current) => ({ ...current, botName: event.target.value }))
                }
                placeholder="Agent name"
                required
              />
            </label>
            <div className="modalActions">
              <button type="button" onClick={() => setShowCreate(false)}>
                Cancel
              </button>
              <button className="confirmButton" disabled={busy || !createForm.botName.trim()}>
                Confirm
              </button>
            </div>
          </form>
        </div>
      )}
    </main>
  );
}

function AdminPage() {
  const [previewMode, setPreviewMode] = useState('ADMN');
  const [users, setUsers] = useState([]);
  const [accessCodeStatus, setAccessCodeStatus] = useState('Generate code');
  const [adminError, setAdminError] = useState('');
  const [busy, setBusy] = useState(false);
  const userSession = loadUserSession();

  async function loadUsers() {
    if (!userSession?.token) {
      setAdminError('Sign in as an admin to view users.');
      return;
    }

    setAdminError('');
    const response = await fetch(`${API_BASE}/users`, {
      headers: authHeaders(userSession),
    });
    const data = await response.json();
    if (!response.ok) {
      throw data;
    }
    setUsers((data.users ?? []).map(normalizeAdminUser));
  }

  useEffect(() => {
    loadUsers().catch((error) => setAdminError(formatError(error)));
  }, []);

  function updateRole(userId, role) {
    setUsers((current) =>
      current.map((user) => (user.id === userId ? { ...user, role } : user)),
    );
  }

  async function refreshAccessCode() {
    if (!userSession?.token) {
      setAdminError('Sign in as an admin to generate an access code.');
      return;
    }

    setBusy(true);
    setAdminError('');
    try {
      const response = await fetch(`${API_BASE}/access-codes`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          ...authHeaders(userSession),
        },
        body: JSON.stringify({ label: 'Meetup access' }),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      setAccessCodeStatus(data.code);
    } catch (error) {
      setAdminError(formatError(error));
    } finally {
      setBusy(false);
    }
  }

  function logOutUser(userId) {
    setUsers((current) =>
      current.map((user) => (user.id === userId ? { ...user, status: 'inactive' } : user)),
    );
  }

  function logOutAll() {
    setUsers((current) => current.map((user) => ({ ...user, status: 'inactive' })));
  }

  function openWorkspace() {
    window.location.href = '/';
  }

  return (
    <main className="adminPage">
      <section className="adminPanel" aria-label="Admin user management preview">
        <header className="adminTopBar">
          <label className="adminPreviewControl">
            <span>Preview:</span>
            <select value={previewMode} onChange={(event) => setPreviewMode(event.target.value)}>
              <option value="ADMN">ADMN</option>
              <option value="PAR">PAR</option>
            </select>
          </label>

          <div className="adminTopActions">
            <button className="adminNavButton" type="button" onClick={openWorkspace}>
              Workspace
            </button>
            <div className="accessCodeControl" aria-label="Access code">
              <code>{accessCodeStatus}</code>
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
            <span>Name</span>
            <span>Email</span>
            <span>Role</span>
            <span>Status</span>
            <button type="button" onClick={logOutAll}>
              Log Out All
            </button>
          </div>

          <div className="adminUserList">
            {users.map((user) => (
              <article className="adminUserRow" key={user.id}>
                <strong>{user.name}</strong>
                <span>{user.email}</span>
                <select
                  value={user.role}
                  onChange={(event) => updateRole(user.id, event.target.value)}
                  aria-label={`Role for ${user.name}`}
                >
                  <option value="ADMN">ADMN</option>
                  <option value="PAR">PAR</option>
                </select>
                <span
                  className={`adminStatusDot ${user.status}`}
                  title={adminStatusLabel(user.status)}
                  aria-label={adminStatusLabel(user.status)}
                />
                <button type="button" onClick={() => logOutUser(user.id)}>
                  Log Out
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

function userStatus(vm, lastActivity, now) {
  const lower = vm.status.toLowerCase();
  if (lower === 'stopped' || lower === 'paused' || lower === 'crashed') {
    return 'inactive';
  }
  if (!lastActivity) {
    return 'inactive';
  }
  return now - lastActivity >= 5 * 60 * 1000 ? 'stagnant' : 'active';
}

function statusLabel(status) {
  if (status === 'active') return 'Active';
  if (status === 'stagnant') return 'Stagnant';
  return 'Inactive';
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
  if (user.status === 'inactive' || !user.last_active_at) return 'inactive';
  const inactiveAfter = 15 * 60;
  const stagnantAfter = 5 * 60;
  const ageSeconds = Math.max(0, Math.floor(Date.now() / 1000) - user.last_active_at);
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

function loadStoredChats() {
  try {
    const stored = window.localStorage.getItem(CHAT_STORAGE_KEY);
    if (!stored) return {};
    const parsed = JSON.parse(stored);
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

function loadUserSession() {
  try {
    const stored = window.localStorage.getItem(USER_SESSION_KEY);
    if (!stored) return null;
    const parsed = JSON.parse(stored);
    return parsed?.email && parsed?.token ? parsed : null;
  } catch {
    return null;
  }
}

function authHeaders(session = loadUserSession()) {
  return session?.token ? { Authorization: `Bearer ${session.token}` } : {};
}

function renderResult(result) {
  const output = [result.stdout, result.stderr].filter(Boolean).join('\n');
  return output || `Done.`;
}

function formatError(error) {
  if (error?.stdout || error?.stderr) {
    return renderResult(error);
  }
  return error?.error ?? String(error);
}

createRoot(document.getElementById('root')).render(<Root />);
