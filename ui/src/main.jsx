import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  ChevronDown,
  ExternalLink,
  PanelLeft,
  Plus,
  RefreshCcw,
  Search,
  Send,
  Sparkles,
  Users,
} from 'lucide-react';
import './styles.css';

const API_BASE = import.meta.env.VITE_API_BASE ?? '/api';

function App() {
  const [vms, setVms] = useState([]);
  const [selectedName, setSelectedName] = useState('');
  const [busy, setBusy] = useState(false);
  const [showCreate, setShowCreate] = useState(false);
  const [createForm, setCreateForm] = useState({ userName: '', botName: '' });
  const [chatInput, setChatInput] = useState('');
  const [messages, setMessages] = useState([]);

  const selectedVm = useMemo(
    () => vms.find((vm) => vm.name === selectedName) ?? vms[0],
    [selectedName, vms],
  );

  useEffect(() => {
    refresh().catch(() => {});
  }, []);

  async function request(path, options = {}) {
    setBusy(true);
    try {
      const response = await fetch(`${API_BASE}${path}`, {
        headers: { 'Content-Type': 'application/json' },
        ...options,
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      return data;
    } finally {
      setBusy(false);
    }
  }

  async function refresh() {
    const nextVms = normalizeVmList(await request('/vms'));
    setVms(nextVms);
    if (nextVms.length && !nextVms.some((vm) => vm.name === selectedName)) {
      setSelectedName(nextVms[0].name);
    }
    if (!nextVms.length) {
      setSelectedName('');
    }
    return nextVms;
  }

  function openAdminPage() {
    window.location.href = '/admin';
  }

  async function createWorkspace(event) {
    event.preventDefault();
    const name = createForm.botName.trim();
    if (!name) return;

    await request('/vms', {
      method: 'POST',
      body: JSON.stringify({ name, replace: true }),
    });
    const nextVms = await refresh();
    const createdWorkspace = findWorkspaceBySubmittedName(nextVms, name);
    setCreateForm({ userName: '', botName: '' });
    setShowCreate(false);
    setSelectedName(createdWorkspace?.name ?? nextVms[0]?.name ?? '');
    setMessages([]);
  }

  async function sendMessage(event) {
    event.preventDefault();
    if (!selectedVm) return;

    const prompt = chatInput.trim();
    if (!prompt) return;

    setChatInput('');
    setMessages((current) => [...current, { role: 'user', content: prompt }]);

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/codex`, {
        method: 'POST',
        body: JSON.stringify({ prompt }),
      });
      setMessages((current) => [...current, { role: 'assistant', content: renderResult(result) }]);
      await refresh();
    } catch (error) {
      setMessages((current) => [...current, { role: 'assistant', content: formatError(error) }]);
    }
  }

  async function launchOpencode() {
    if (!selectedVm) return;

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/opencode`, {
        method: 'POST',
      });
      const url = launchUrlFromResult(result);
      if (url) {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
      await refresh();
    } catch (error) {
      setMessages((current) => [...current, { role: 'assistant', content: formatError(error) }]);
    }
  }

  async function launchHermes() {
    if (!selectedVm) return;

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/hermes-ui`, {
        method: 'POST',
      });
      const url = launchUrlFromResult(result);
      if (url) {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
      await refresh();
    } catch (error) {
      setMessages((current) => [...current, { role: 'assistant', content: formatError(error) }]);
    }
  }

  function selectWorkspace(name) {
    setSelectedName(name);
    setMessages([]);
  }

  return (
    <main className="appShell">
      <aside className="sidebar">
        <div className="brandRow">
          <div className="brandMark">A</div>
          <button className="brandButton">
            Agent Mom
            <ChevronDown size={16} />
          </button>
        </div>

        <button className="createButton" onClick={() => setShowCreate(true)}>
          <Plus size={18} />
          Create
        </button>

        <button className="sidebarAction">
          <Search size={18} />
          Search workspaces
        </button>

        <div className="workspaceSection">
          <h2>Today</h2>
          <div className="workspaceList">
            {vms.map((vm) => (
              <button
                key={vm.name}
                className={`workspaceItem ${selectedVm?.name === vm.name ? 'active' : ''}`}
                onClick={() => selectWorkspace(vm.name)}
              >
                <span>{workspaceDisplayName(vm)}</span>
                <small>{friendlyStatus(vm.status)}</small>
              </button>
            ))}
            {!vms.length && <p className="emptyList">No workspaces yet.</p>}
          </div>
        </div>

        <div className="sessionBox">
          <h2>Session</h2>
          <strong>Local workspace</strong>
          <span>Signed in on this machine.</span>
        </div>
      </aside>

      <section className="chatShell">
        <header className="chatHeader">
          <button className="squareButton" title="Toggle sidebar">
            <PanelLeft size={20} />
          </button>
          <div>
            <h1>{selectedVm ? workspaceDisplayName(selectedVm) : 'Agent workspace'}</h1>
            <p>{selectedVm ? friendlyStatus(selectedVm.status) : 'Create a workspace to begin.'}</p>
          </div>
          <div className="headerActions">
            {userSession.role === 'ADMN' && (
              <button className="refreshButton" type="button" onClick={openAdminPage}>
                <Users size={17} />
                Admin
              </button>
            )}
            <button className="refreshButton" onClick={refresh} disabled={busy}>
              <RefreshCcw size={17} />
              Refresh
            </button>
            <button
              className="refreshButton"
              onClick={launchOpencode}
              disabled={!selectedVm || busy}
            >
              <ExternalLink size={17} />
              OpenCode
            </button>
            <button className="refreshButton" onClick={launchHermes} disabled={!selectedVm || busy}>
              <ExternalLink size={17} />
              Hermes
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
              <h2>Create your bot</h2>
              <p>Name the bot you want to chat with.</p>
            </div>
            <label>
              <span>Your name</span>
              <input
                value={createForm.userName}
                onChange={(event) =>
                  setCreateForm((current) => ({ ...current, userName: event.target.value }))
                }
                placeholder="Your name"
                autoFocus
              />
            </label>
            <label>
              <span>Bot name</span>
              <input
                value={createForm.botName}
                onChange={(event) =>
                  setCreateForm((current) => ({ ...current, botName: event.target.value }))
                }
                placeholder="Bot name"
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

function AdminPage({ userSession }) {
  const [users, setUsers] = useState([]);
  const [accessCodeStatus, setAccessCodeStatus] = useState('Generate code');
  const [adminError, setAdminError] = useState('');
  const [busy, setBusy] = useState(false);

  function endAdminSession() {
    window.localStorage.removeItem(USER_SESSION_KEY);
    window.location.href = '/';
  }

  function leaveAdminView(updatedSession = userSession) {
    window.localStorage.setItem(USER_SESSION_KEY, JSON.stringify(updatedSession));
    window.location.href = '/';
  }

  async function loadUsers() {
    if (!userSession?.token) {
      endAdminSession();
      return;
    }

    setAdminError('');
    const response = await fetch(`${API_BASE}/users`, {
      headers: authHeaders(userSession),
    });
    const data = await response.json();
    if (!response.ok) {
      if (response.status === 401 || response.status === 403) {
        endAdminSession();
        return;
      }
      throw data;
    }
    setUsers((data.users ?? []).map(normalizeAdminUser));
  }

  useEffect(() => {
    loadUsers().catch((error) => setAdminError(formatError(error)));
  }, [userSession?.token]);

  useEffect(() => {
    if (!userSession?.token) {
      endAdminSession();
      return undefined;
    }

    let cancelled = false;
    const checkAdminSession = async () => {
      try {
        const refreshedSession = await validateSession(userSession);
        if (cancelled) return;
        if (refreshedSession.role !== 'ADMN') {
          leaveAdminView(refreshedSession);
        }
      } catch {
        if (!cancelled) {
          endAdminSession();
        }
      }
    };

    const interval = window.setInterval(checkAdminSession, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [userSession?.token]);

  async function updateRole(userId, role) {
    if (!userSession?.token) {
      setAdminError('Sign in as an admin to update roles.');
      return;
    }

    const previousUsers = users;
    setUsers((current) =>
      current.map((user) => (user.id === userId ? { ...user, role } : user)),
    );
    setAdminError('');

    try {
      const response = await fetch(`${API_BASE}/users/${userId}/role`, {
        method: 'PATCH',
        headers: {
          'Content-Type': 'application/json',
          ...authHeaders(userSession),
        },
        body: JSON.stringify({ role }),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      const updatedUser = normalizeAdminUser(data);
      setUsers((current) =>
        current.map((user) => (user.id === updatedUser.id ? updatedUser : user)),
      );
      if (sameEmail(updatedUser.email, userSession.email)) {
        const updatedSession = { ...userSession, role: updatedUser.role };
        if (updatedUser.role !== 'ADMN') {
          leaveAdminView(updatedSession);
        } else {
          window.localStorage.setItem(USER_SESSION_KEY, JSON.stringify(updatedSession));
        }
      }
    } catch (error) {
      setUsers(previousUsers);
      setAdminError(formatError(error));
    }
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

  async function logOutUser(user) {
    if (!userSession?.token) {
      setAdminError('Sign in as an admin to log out users.');
      return;
    }

    const previousUsers = users;
    setUsers((current) =>
      current.map((currentUser) =>
        currentUser.id === user.id ? { ...currentUser, status: 'inactive' } : currentUser,
      ),
    );
    setAdminError('');

    try {
      const response = await fetch(`${API_BASE}/users/${user.id}/sessions`, {
        method: 'DELETE',
        headers: authHeaders(userSession),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      if (sameEmail(user.email, userSession.email)) {
        endAdminSession();
      }
    } catch (error) {
      setUsers(previousUsers);
      setAdminError(formatError(error));
    }
  }

  async function logOutAll() {
    if (!userSession?.token) {
      setAdminError('Sign in as an admin to log out users.');
      return;
    }

    const previousUsers = users;
    setUsers((current) => current.map((user) => ({ ...user, status: 'inactive' })));
    setAdminError('');

    try {
      const response = await fetch(`${API_BASE}/sessions`, {
        method: 'DELETE',
        headers: authHeaders(userSession),
      });
      const data = await response.json();
      if (!response.ok) {
        throw data;
      }
      window.localStorage.removeItem(USER_SESSION_KEY);
      window.location.href = '/';
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
                <button type="button" onClick={() => logOutUser(user)}>
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

function workspaceDisplayName(workspace) {
  return workspace.display_name ?? workspace.displayName ?? workspace.name ?? 'Agent workspace';
}

function normalizeVmList(data) {
  if (Array.isArray(data)) return data;
  if (Array.isArray(data?.vms)) return data.vms;
  return [];
}

function findWorkspaceBySubmittedName(workspaces, submittedName) {
  return workspaces.find((workspace) =>
    [workspace.name, workspace.slug, workspace.display_name, workspace.displayName].includes(
      submittedName,
    ),
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

createRoot(document.getElementById('root')).render(<App />);
