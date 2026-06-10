import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  ChevronDown,
  PanelLeft,
  Plus,
  RefreshCcw,
  Search,
  Send,
  Sparkles,
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
    const data = await request('/vms');
    setVms(data.vms);
    if (data.vms.length && !data.vms.some((vm) => vm.name === selectedName)) {
      setSelectedName(data.vms[0].name);
    }
    if (!data.vms.length) {
      setSelectedName('');
    }
  }

  async function createWorkspace(event) {
    event.preventDefault();
    const name = createForm.botName.trim();
    if (!name) return;

    await request('/vms', {
      method: 'POST',
      body: JSON.stringify({ name, replace: true }),
    });
    setCreateForm({ userName: '', botName: '' });
    setShowCreate(false);
    setSelectedName(name);
    setMessages([]);
    await refresh();
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
                <span>{vm.name}</span>
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
            <h1>{selectedVm?.name ?? 'Agent workspace'}</h1>
            <p>{selectedVm ? friendlyStatus(selectedVm.status) : 'Create a workspace to begin.'}</p>
          </div>
          <button className="refreshButton" onClick={refresh} disabled={busy}>
            <RefreshCcw size={17} />
            Refresh
          </button>
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

function friendlyStatus(status) {
  const lower = status.toLowerCase();
  if (lower === 'running' || lower === 'draining') return 'Ready';
  if (lower === 'stopped') return 'Paused';
  if (lower === 'paused') return 'Paused';
  if (lower === 'crashed') return 'Needs attention';
  return status;
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
