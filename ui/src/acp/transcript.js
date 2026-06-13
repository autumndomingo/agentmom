export function buildTranscript(events) {
  const items = [];
  let assistant = null;
  let thinking = null;
  const toolsById = new Map();

  for (const event of events ?? []) {
    const payload = event.payload ?? {};

    if (
      event.kind === 'startup.failed' ||
      event.kind === 'startup.preflight_failed' ||
      event.kind === 'transport.error'
    ) {
      items.push(chatItem(event, 'system', 'Hermes ACP failed', formatErrorText(payload.error), payload));
      continue;
    }

    if (event.kind === 'process.exited') {
      items.push(chatItem(event, 'system', 'Hermes ACP exited', 'The adapter exited. Agent Mom will restart it on the next poll.', payload));
      assistant = null;
      thinking = null;
      continue;
    }

    if (event.direction === 'out' && event.kind === 'session/prompt') {
      const promptText = extractPromptText(payload.params ?? {});
      if (promptText) {
        items.push(chatItem(event, 'user', 'You', promptText, payload.params));
        assistant = null;
        thinking = null;
      }
      continue;
    }

    if (event.kind === 'session/update') {
      const update = payload.params?.update ?? payload.params ?? {};
      const type = update.sessionUpdate ?? update.session_update ?? update.type ?? 'session/update';
      const text = extractText(update.content ?? {}) || extractText(update);

      if (type.includes('user_message')) {
        if (text) items.push(chatItem(event, 'user', 'You', text, update));
        assistant = null;
        thinking = null;
        continue;
      }

      if (type.includes('agent_message')) {
        if (!assistant) {
          assistant = chatItem(event, 'assistant', 'Hermes', '');
          items.push(assistant);
        }
        assistant.text = `${assistant.text ?? ''}${text}`;
        continue;
      }

      if (type.includes('thought')) {
        if (!thinking) {
          thinking = chatItem(event, 'thinking', 'Thinking', '');
          items.push(thinking);
        }
        thinking.text = `${thinking.text ?? ''}${text}`;
        continue;
      }

      if (type.includes('tool')) {
        assistant = null;
        thinking = null;
        const card = toolCard(event, update);
        const toolId = update.toolCallId ?? update.tool_call_id;
        if (toolId && toolsById.has(toolId)) {
          const previous = toolsById.get(toolId);
          previous.title = card.title;
          previous.text = [previous.text, card.text].filter(Boolean).join('\n');
          previous.raw = { ...(previous.raw ?? {}), ...(card.raw ?? {}) };
        } else {
          if (toolId) toolsById.set(toolId, card);
          items.push(card);
        }
        continue;
      }

      if (type.includes('plan')) {
        items.push(chatItem(event, 'tool', 'Plan', planText(update), update));
        continue;
      }

      if (type.includes('usage') || type.includes('session_info') || type.includes('available_commands')) {
        continue;
      }
    }

    if (event.kind.includes('permission')) {
      items.push(chatItem(event, 'system', 'Permission requested', null, payload));
      continue;
    }

    if (event.direction === 'in' && payload.method && payload.id != null) {
      items.push(chatItem(event, 'tool', `${payload.method} request`, extractText(payload.params ?? {}), payload.params));
      continue;
    }

    if (event.direction === 'out' && (event.kind === 'rpc.response' || event.kind === 'rpc.error_response')) {
      const result = payload.result ?? payload.error ?? payload;
      const label = payload.error ? 'Client error' : 'Client response';
      items.push(chatItem(event, 'tool', label, extractText(result), result));
    }
  }

  return {
    items: items.filter((item) => item.text || item.raw || item.role === 'system'),
  };
}

export function extractText(value) {
  if (!value) return '';
  if (typeof value === 'string') return value;
  if (Array.isArray(value)) return value.map(extractText).filter(Boolean).join('\n');
  if (typeof value !== 'object') return String(value);
  if (typeof value.text === 'string') return value.text;
  if (typeof value.delta === 'string') return value.delta;
  if (typeof value.content === 'string') return value.content;
  if (value.type === 'image') return `[Image: ${value.mimeType ?? value.mime_type ?? 'image'}]`;
  if (value.type === 'audio') return `[Audio: ${value.mimeType ?? value.mime_type ?? 'audio'}]`;
  if (value.type === 'resource') return extractResourceText(value.resource);
  if (value.content) return extractText(value.content);
  return '';
}

function chatItem(event, role, title, text, raw = null) {
  return { key: `${event.seq}-${title}`, role, title, text, raw };
}

function extractPromptText(params) {
  return extractText(params.prompt) || extractText(params.content);
}

function toolCard(event, update) {
  const title = update.title ?? update.name ?? update.toolCallId ?? update.tool_call_id ?? 'Tool';
  const kind = update.kind ?? update.status ?? 'tool';
  const text = extractText(update.content) || update.text || update.delta || '';
  return chatItem(event, 'tool', `${title} · ${kind}`, text, update);
}

function planText(update) {
  const entries = update.entries ?? update.plan ?? update.steps ?? [];
  if (Array.isArray(entries)) {
    return entries
      .map((entry) => {
        if (typeof entry === 'string') return entry;
        return [entry.status, entry.title ?? entry.text ?? entry.step].filter(Boolean).join(' ');
      })
      .filter(Boolean)
      .join('\n');
  }
  return extractText(entries);
}

function extractResourceText(resource) {
  if (!resource) return '';
  const name = resource.name ?? resource.title ?? '';
  if (typeof resource.text === 'string') return [name, resource.text].filter(Boolean).join('\n');
  if (typeof resource.uri === 'string') return [name, resource.uri].filter(Boolean).join('\n');
  if (typeof resource.blob === 'string') {
    const mimeType = resource.mimeType ?? resource.mime_type ?? 'application/octet-stream';
    return `[Resource${name ? `: ${name}` : ''}: ${mimeType}]`;
  }
  return extractText(resource);
}

function formatErrorText(error) {
  if (!error) return 'Unknown error';
  if (typeof error === 'string') return error;
  return error.message ?? error.error ?? JSON.stringify(error);
}
