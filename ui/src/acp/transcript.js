export function buildTranscript(events) {
  const items = [];
  let assistant = null;
  let thinking = null;
  const toolsById = new Map();

  for (const event of events ?? []) {
    const message = event.message ?? {};
    if (message.method === 'mom/status') {
      const params = message.params ?? {};
      if (params.state === 'error') {
        items.push(chatItem(event, 'system', 'Hermes ACP error', params.message ?? 'Connection failed', params));
      }
      continue;
    }

    if (event.direction === 'out' && message.method === 'session/prompt') {
      const promptText = extractText(message.params?.prompt);
      if (promptText) {
        items.push(chatItem(event, 'user', 'You', promptText, message.params));
        assistant = null;
        thinking = null;
      }
      continue;
    }

    if (message.method === 'session/update') {
      const update = message.params?.update ?? message.params ?? {};
      const type = update.sessionUpdate ?? update.session_update ?? update.type ?? '';
      const text = extractText(update.content ?? update);

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
        assistant = null;
        thinking = null;
        continue;
      }
    }

    if (message.method?.includes('permission')) {
      items.push(chatItem(event, 'system', 'Permission requested', null, message.params));
    }
  }

  return {
    items: items.filter((item) => item.text || item.raw || item.role === 'system'),
  };
}

export function buildPendingPermissions(events) {
  const pending = new Map();
  for (const event of events ?? []) {
    const message = event.message ?? {};
    if (event.direction === 'in' && message.id != null && message.method?.includes('permission')) {
      pending.set(String(message.id), {
        id: String(message.id),
        method: message.method,
        params: message.params ?? {},
      });
    }
    if (event.direction === 'out' && message.id != null && (message.result || message.error)) {
      pending.delete(String(message.id));
    }
  }
  return [...pending.values()];
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
  if (value.type === 'resource') return extractText(value.resource);
  if (value.content) return extractText(value.content);
  return '';
}

function chatItem(event, role, title, text, raw = null) {
  return { key: `${event.seq}-${title}`, role, title, text, raw };
}

function toolCard(event, update) {
  const title = update.title ?? update.name ?? update.toolCallId ?? update.tool_call_id ?? 'Tool';
  const kind = update.kind ?? update.status ?? 'tool';
  const text = extractText(update.content) || update.text || update.delta || '';
  return chatItem(event, 'tool', `${title} · ${kind}`, text, update);
}
