import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  ChevronDown,
  Edit3,
  ExternalLink,
  PanelLeft,
  Plus,
  RefreshCcw,
  Rocket,
  Send,
  Sparkles,
} from 'lucide-react';
import './styles.css';

const API_BASE = import.meta.env.VITE_API_BASE ?? '/api';
const CHAT_STORAGE_KEY = 'agent-mom-chats';

function App() {
  const [vms, setVms] = useState([]);
  const [selectedName, setSelectedName] = useState('');
  const [busy, setBusy] = useState(false);
  const [showUsers, setShowUsers] = useState(false);
  const [showCreate, setShowCreate] = useState(false);
  const [createForm, setCreateForm] = useState({ userName: '', botName: '' });
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
    refresh().catch(() => {});
  }, []);

  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(interval);
  }, []);

  useEffect(() => {
    window.localStorage.setItem(CHAT_STORAGE_KEY, JSON.stringify(chatsByUser));
  }, [chatsByUser]);

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
    markActive(name);
    startNewChat(name);
    await refresh();
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

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/codex`, {
        method: 'POST',
        body: JSON.stringify({ prompt }),
      });
      appendMessage(selectedVm.name, chatId, { role: 'assistant', content: renderResult(result) });
      await refresh();
    } catch (error) {
      appendMessage(selectedVm.name, chatId, { role: 'assistant', content: formatError(error) });
    }
  }

  async function launchOpencode() {
    if (!selectedVm) return;

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/opencode`, {
        method: 'POST',
      });
      const url = result.stdout.trim().split(/\s+/).at(-1);
      if (url) {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
      await refresh();
    } catch (error) {
      const chatId = ensureChatForPrompt(selectedVm.name, 'OpenCode');
      appendMessage(selectedVm.name, chatId, { role: 'assistant', content: formatError(error) });
    }
  }

  async function launchHermes() {
    if (!selectedVm) return;

    try {
      const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/hermes-ui`, {
        method: 'POST',
      });
      const url = result.stdout.trim().split(/\s+/).at(-1);
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

  return (
    <main className="appShell">
      <aside className="sidebar">
        <div className="sidebarTopBar">
          <button className="sidebarIconButton" title="Toggle sidebar">
            <PanelLeft size={24} />
          </button>
        </div>

        <div className="sidebarQuickActions">
          <button className="newChatButton" onClick={() => startNewChat()} disabled={!selectedVm}>
            <Edit3 size={29} strokeWidth={2.25} />
            New Chat
          </button>

          <button className="launchButton" onClick={() => setShowCreate(true)}>
            <Rocket size={31} strokeWidth={2.05} />
            Create new user
          </button>
        </div>

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
                    <span>{vm.name}</span>
                    <small>{statusLabel(status)}</small>
                  </button>
                );
              })}
              {!vms.length && <p className="emptyList">No users yet.</p>}
            </div>
          )}
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
          {selectedVm && !selectedChats.length && (
            <p className="emptyList">New chats for {selectedVm.name} will appear here.</p>
          )}
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
          <div className="headerActions">
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
