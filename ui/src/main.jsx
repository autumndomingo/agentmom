import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  Activity,
  Cpu,
  Play,
  Plus,
  RefreshCcw,
  Square,
  Stethoscope,
  Terminal,
  Trash2,
  WandSparkles,
} from 'lucide-react';
import './styles.css';

const API_BASE = import.meta.env.VITE_API_BASE ?? '/api';

function App() {
  const [vms, setVms] = useState([]);
  const [selectedName, setSelectedName] = useState('');
  const [includeAll, setIncludeAll] = useState(false);
  const [busy, setBusy] = useState(false);
  const [createForm, setCreateForm] = useState({
    name: '',
    cpus: 2,
    memory: 2048,
    replace: true,
    rebuild_snapshot: false,
    no_snapshot: false,
  });
  const [execCommand, setExecCommand] = useState('pwd');
  const [codexPrompt, setCodexPrompt] = useState('Reply exactly ok');
  const [hermesArgs, setHermesArgs] = useState('--help');
  const [log, setLog] = useState('Ready.');

  const selectedVm = useMemo(
    () => vms.find((vm) => vm.name === selectedName) ?? vms[0],
    [selectedName, vms],
  );

  useEffect(() => {
    refresh({ showOutput: true }).catch(() => {});
  }, [includeAll]);

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
    } catch (error) {
      setLog(formatError(error));
      throw error;
    } finally {
      setBusy(false);
    }
  }

  async function refresh({ showOutput = true } = {}) {
    const data = await request(`/vms${includeAll ? '?all=true' : ''}`);
    setVms(data.vms);
    if (data.vms.length && !data.vms.some((vm) => vm.name === selectedName)) {
      setSelectedName(data.vms[0].name);
    }
    if (!data.vms.length) {
      setSelectedName('');
    }
    if (showOutput) {
      setLog(renderResult(data.raw));
    }
  }

  async function createVm(event) {
    event.preventDefault();
    const result = await request('/vms', {
      method: 'POST',
      body: JSON.stringify({
        ...createForm,
        cpus: Number(createForm.cpus),
        memory: Number(createForm.memory),
      }),
    });
    setLog(renderResult(result));
    await refresh({ showOutput: false });
  }

  async function vmAction(action) {
    if (!selectedVm) return;
    const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/${action}`, {
      method: 'POST',
      body: '{}',
    });
    setLog(renderResult(result));
    await refresh({ showOutput: false });
  }

  async function runCommand(kind) {
    if (!selectedVm) return;
    const payloads = {
      exec: { command: splitArgs(execCommand) },
      codex: { prompt: codexPrompt },
      hermes: { command: splitArgs(hermesArgs) },
      doctor: {},
    };
    const result = await request(`/vms/${encodeURIComponent(selectedVm.name)}/${kind}`, {
      method: 'POST',
      body: JSON.stringify(payloads[kind]),
    });
    setLog(renderResult(result));
    await refresh({ showOutput: false });
  }

  return (
    <main className="shell">
      <header className="topbar">
        <div>
          <div className="statusPill">
            <WandSparkles size={18} />
            Agent Mom control
          </div>
          <h1>Agent Mom</h1>
          <p>Microsandbox VM control panel</p>
        </div>
        <button className="iconButton" onClick={refresh} disabled={busy} title="Refresh">
          <RefreshCcw size={18} />
        </button>
      </header>

      <section className="layout">
        <aside className="sidebar">
          <div className="sectionHeader">
            <h2>VMs</h2>
            <label className="toggle">
              <input
                type="checkbox"
                checked={includeAll}
                onChange={(event) => setIncludeAll(event.target.checked)}
              />
              all
            </label>
          </div>

          <div className="vmList">
            {vms.map((vm) => (
              <button
                key={vm.name}
                className={`vmItem ${selectedVm?.name === vm.name ? 'active' : ''}`}
                onClick={() => setSelectedName(vm.name)}
              >
                <span className={`status ${vm.status.toLowerCase()}`} />
                <span>
                  <strong>{vm.name}</strong>
                  <small>{vm.status}</small>
                </span>
              </button>
            ))}
            {!vms.length && <p className="empty">No VMs found.</p>}
          </div>

          <form className="createForm" onSubmit={createVm}>
            <h2>Create VM</h2>
            <input
              placeholder="name"
              value={createForm.name}
              onChange={(event) => setCreateForm({ ...createForm, name: event.target.value })}
              required
            />
            <div className="numberGrid">
              <label>
                <span>CPUs</span>
                <input
                  type="number"
                  min="1"
                  max="16"
                  value={createForm.cpus}
                  onChange={(event) => setCreateForm({ ...createForm, cpus: event.target.value })}
                />
              </label>
              <label>
                <span>MiB</span>
                <input
                  type="number"
                  min="512"
                  step="256"
                  value={createForm.memory}
                  onChange={(event) => setCreateForm({ ...createForm, memory: event.target.value })}
                />
              </label>
            </div>
            <label className="check">
              <input
                type="checkbox"
                checked={createForm.replace}
                onChange={(event) => setCreateForm({ ...createForm, replace: event.target.checked })}
              />
              replace existing
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={createForm.rebuild_snapshot}
                onChange={(event) =>
                  setCreateForm({ ...createForm, rebuild_snapshot: event.target.checked })
                }
              />
              rebuild snapshot
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={createForm.no_snapshot}
                onChange={(event) =>
                  setCreateForm({ ...createForm, no_snapshot: event.target.checked })
                }
              />
              no snapshot
            </label>
            <button className="primary" disabled={busy}>
              <Plus size={16} />
              Create
            </button>
          </form>
        </aside>

        <section className="workspace">
          <div className="panel vmPanel">
            <div>
              <h2>{selectedVm?.name ?? 'No VM selected'}</h2>
              <p>{selectedVm?.image ?? 'Create or select a VM to begin.'}</p>
            </div>
            <div className="actions">
              <button onClick={() => vmAction('start')} disabled={!selectedVm || busy} title="Start">
                <Play size={17} />
              </button>
              <button onClick={() => vmAction('stop')} disabled={!selectedVm || busy} title="Stop">
                <Square size={17} />
              </button>
              <button onClick={() => runCommand('doctor')} disabled={!selectedVm || busy} title="Doctor">
                <Stethoscope size={17} />
              </button>
              <button onClick={() => vmAction('remove')} disabled={!selectedVm || busy} title="Remove">
                <Trash2 size={17} />
              </button>
            </div>
          </div>

          <div className="tools">
            <CommandBox
              icon={<Terminal size={17} />}
              label="Exec"
              value={execCommand}
              onChange={setExecCommand}
              onRun={() => runCommand('exec')}
              disabled={!selectedVm || busy}
            />
            <CommandBox
              icon={<WandSparkles size={17} />}
              label="Codex"
              value={codexPrompt}
              onChange={setCodexPrompt}
              onRun={() => runCommand('codex')}
              disabled={!selectedVm || busy}
            />
            <CommandBox
              icon={<Cpu size={17} />}
              label="Hermes"
              value={hermesArgs}
              onChange={setHermesArgs}
              onRun={() => runCommand('hermes')}
              disabled={!selectedVm || busy}
            />
          </div>

          <section className="console">
            <div className="consoleHeader">
              <span>
                <Activity size={16} />
                Output
              </span>
              {busy && <b>running</b>}
            </div>
            <pre>{log}</pre>
          </section>
        </section>
      </section>
    </main>
  );
}

function CommandBox({ icon, label, value, onChange, onRun, disabled }) {
  return (
    <form
      className="commandBox"
      onSubmit={(event) => {
        event.preventDefault();
        onRun();
      }}
    >
      <label>
        <span>
          {icon}
          {label}
        </span>
        <input value={value} onChange={(event) => onChange(event.target.value)} />
      </label>
      <button disabled={disabled}>
        <Play size={16} />
      </button>
    </form>
  );
}

function splitArgs(value) {
  return value.match(/(?:[^\s"]+|"[^"]*")+/g)?.map((part) => part.replace(/^"|"$/g, '')) ?? [];
}

function renderResult(result) {
  const output = [result.stdout, result.stderr].filter(Boolean).join('\n');
  return output || `exit ${result.code ?? 0}`;
}

function formatError(error) {
  if (error?.stdout || error?.stderr) {
    return renderResult(error);
  }
  return error?.error ?? String(error);
}

createRoot(document.getElementById('root')).render(<App />);
