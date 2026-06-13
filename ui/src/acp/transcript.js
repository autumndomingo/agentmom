export function buildTranscript(events) {
  const items = [];
  const openChunks = new Map();
  const toolsById = new Map();

  for (const event of events ?? []) {
    const message = event.message ?? {};

    if (message.method === 'mom/status') {
      const params = message.params ?? {};
      if (params.state === 'error') {
        items.push(chatItem(event, 'system', 'Hermes ACP error', [
          textBlock(params.message ?? 'Connection failed'),
          jsonBlock(params, 'Status payload'),
        ]));
      }
      continue;
    }

    if (event.direction === 'out' && message.method === 'session/prompt') {
      const blocks = blocksFromContent(message.params?.prompt);
      if (hasDisplayBlocks(blocks)) {
        items.push(chatItem(event, 'user', 'You', blocks, message.params));
        clearOpenChunks(openChunks);
      }
      continue;
    }

    if (message.method === 'session/update') {
      const update = message.params?.update ?? message.params ?? {};
      const type = update.sessionUpdate ?? update.session_update ?? update.type ?? '';

      if (type === 'user_message_chunk') {
        appendChunk(items, openChunks, event, 'user', 'You', update);
        continue;
      }
      if (type === 'agent_message_chunk') {
        appendChunk(items, openChunks, event, 'assistant', 'Hermes', update);
        continue;
      }
      if (type === 'agent_thought_chunk') {
        appendChunk(items, openChunks, event, 'thinking', 'Thinking', update);
        continue;
      }
      if (type === 'tool_call' || type === 'tool_call_update' || type.includes('tool')) {
        const card = toolCard(event, update);
        const toolId = update.toolCallId ?? update.tool_call_id;
        if (toolId && toolsById.has(toolId)) {
          mergeToolCard(toolsById.get(toolId), card);
        } else {
          if (toolId) toolsById.set(toolId, card);
          items.push(card);
        }
        clearOpenChunks(openChunks);
        continue;
      }
      if (type === 'plan') {
        items.push(chatItem(event, 'plan', 'Plan', planBlocks(update), update));
        continue;
      }
      if (
        type === 'usage_update' ||
        type === 'available_commands_update' ||
        type === 'session_info_update' ||
        type === 'current_mode_update' ||
        type === 'config_option_update'
      ) {
        continue;
      }

      items.push(chatItem(event, 'system', type || 'Session update', [
        jsonBlock(update, 'Raw session update'),
      ], update));
      continue;
    }

    if (message.method?.includes('permission')) {
      items.push(chatItem(event, 'system', 'Permission requested', permissionBlocks(message.params), message.params));
      continue;
    }

    const response = responseItem(event, message);
    if (response) {
      items.push(response);
      continue;
    }

    if (message.method && event.direction === 'in') {
      items.push(chatItem(event, 'system', `ACP request: ${message.method}`, [
        jsonBlock(message.params ?? {}, 'Request params'),
      ], message.params));
    }
  }

  return {
    items: items.filter((item) => hasDisplayBlocks(item.blocks) || item.raw || item.role === 'system'),
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
    if (event.direction === 'out' && message.id != null && (hasOwn(message, 'result') || message.error)) {
      pending.delete(String(message.id));
    }
  }
  return [...pending.values()];
}

export function blocksFromContent(value) {
  if (value == null) return [];
  if (Array.isArray(value)) return value.flatMap(blocksFromContent);
  if (typeof value === 'string') return [textBlock(value)];
  if (typeof value !== 'object') return [textBlock(String(value))];

  if (value.type === 'content') return blocksFromContent(value.content);
  if (value.type === 'diff') {
    return [{
      type: 'diff',
      path: value.path ?? 'file',
      oldText: value.oldText ?? value.old_text ?? '',
      newText: value.newText ?? value.new_text ?? '',
    }];
  }
  if (value.type === 'terminal') {
    return [{ type: 'terminal', terminalId: value.terminalId ?? value.terminal_id ?? 'terminal' }];
  }
  if (value.type === 'text') return [textBlock(value.text ?? '')];
  if (value.type === 'image') {
    const mime = value.mimeType ?? value.mime_type ?? 'image/png';
    return [{
      type: 'image',
      src: value.data ? dataUrl(mime, value.data) : value.uri,
      label: value.uri ?? mime,
      mime,
    }];
  }
  if (value.type === 'audio') {
    const mime = value.mimeType ?? value.mime_type ?? 'audio/mpeg';
    return [{
      type: 'audio',
      src: value.data ? dataUrl(mime, value.data) : value.uri,
      label: value.uri ?? mime,
      mime,
    }];
  }
  if (value.type === 'resource_link') {
    return [{
      type: 'resource',
      title: value.title ?? value.name ?? value.uri ?? 'Resource',
      uri: value.uri,
      description: value.description,
      mime: value.mimeType ?? value.mime_type,
      size: value.size,
    }];
  }
  if (value.type === 'resource') return blocksFromResource(value.resource);
  if (value.content) return blocksFromContent(value.content);
  if (typeof value.text === 'string') return [textBlock(value.text)];
  if (typeof value.delta === 'string') return [textBlock(value.delta)];

  return [];
}

export function extractText(value) {
  return blocksToText(blocksFromContent(value));
}

function appendChunk(items, openChunks, event, role, title, update) {
  const messageId = update.messageId ?? update.message_id ?? role;
  const key = `${role}:${messageId}`;
  const blocks = blocksFromContent(update.content ?? update);
  if (!hasDisplayBlocks(blocks)) return;

  let item = openChunks.get(key);
  if (!item) {
    item = chatItem(event, role, title, [], update);
    openChunks.set(key, item);
    items.push(item);
  }
  item.blocks = appendBlocks(item.blocks, blocks);
  item.raw = update;
}

function clearOpenChunks(openChunks) {
  openChunks.clear();
}

function chatItem(event, role, title, blocks = [], raw = null) {
  return { key: `${event.seq}-${title}`, role, title, blocks, raw };
}

function toolCard(event, update) {
  const title = update.title ?? update.name ?? update.toolCallId ?? update.tool_call_id ?? 'Tool';
  const metadata = {};
  if (update.kind) metadata.Kind = update.kind;
  if (update.status) metadata.Status = update.status;
  const locations = update.locations ?? [];
  if (locations.length) {
    metadata.Locations = locations.map((location) =>
      [location.path, location.line != null ? `:${location.line}` : ''].join(''),
    ).join(', ');
  }

  const blocks = [
    Object.keys(metadata).length ? metaBlock(metadata) : null,
    ...blocksFromContent(update.content),
    update.rawInput != null || update.raw_input != null
      ? jsonBlock(update.rawInput ?? update.raw_input, 'Raw input')
      : null,
    update.rawOutput != null || update.raw_output != null
      ? jsonBlock(update.rawOutput ?? update.raw_output, 'Raw output')
      : null,
  ].filter(Boolean);

  if (!blocks.length) blocks.push(jsonBlock(update, 'Tool payload'));
  return chatItem(event, 'tool', title, blocks, update);
}

function mergeToolCard(previous, next) {
  previous.title = next.title;
  previous.blocks = appendBlocks(previous.blocks, next.blocks);
  previous.raw = { ...(previous.raw ?? {}), ...(next.raw ?? {}) };
}

function responseItem(event, message) {
  if (message.id == null || (!hasOwn(message, 'result') && !message.error)) return null;
  if (message.error) {
    return chatItem(event, 'system', 'ACP error', [jsonBlock(message.error, 'Error')], message.error);
  }

  return null;
}

function hasOwn(value, key) {
  return Object.prototype.hasOwnProperty.call(value ?? {}, key);
}

function initializeBlocks(result = {}) {
  const agent = result.agentInfo ?? result.agent_info ?? {};
  const caps = result.agentCapabilities ?? result.agent_capabilities ?? {};
  const authMethods = result.authMethods ?? result.auth_methods ?? [];
  const blocks = [
    metaBlock({
      Agent: [agent.title ?? agent.name, agent.version].filter(Boolean).join(' '),
      Protocol: result.protocolVersion ?? result.protocol_version,
    }),
    capabilityBlock(caps),
  ];
  if (authMethods.length) {
    blocks.push(listBlock('Auth methods', authMethods.map((method) =>
      [method.name ?? method.id, method.type].filter(Boolean).join(' / '),
    )));
  }
  return blocks.filter(Boolean);
}

function sessionResultBlocks(result = {}) {
  const blocks = [];
  if (result.sessionId ?? result.session_id) {
    blocks.push(metaBlock({ Session: result.sessionId ?? result.session_id }));
  }
  if (result.models) blocks.push(modelStateBlock(result.models));
  if (result.modes) blocks.push(modeStateBlock(result.modes));
  if (result.configOptions ?? result.config_options) {
    blocks.push(...configOptionBlocks({ configOptions: result.configOptions ?? result.config_options }));
  }
  return blocks.length ? blocks : [jsonBlock(result, 'Session result')];
}

function promptResultBlocks(result = {}) {
  const usage = result.usage ? usageSummary(result.usage) : null;
  return [
    metaBlock({
      Stop: result.stopReason ?? result.stop_reason ?? 'unknown',
      ...(result.userMessageId || result.user_message_id
        ? { 'User message': result.userMessageId ?? result.user_message_id }
        : {}),
      ...(usage ? { Usage: usage } : {}),
    }),
  ];
}

function capabilityBlock(caps = {}) {
  const prompt = caps.promptCapabilities ?? caps.prompt_capabilities ?? {};
  const mcp = caps.mcpCapabilities ?? caps.mcp_capabilities ?? {};
  const sessions = caps.sessionCapabilities ?? caps.session_capabilities ?? {};
  return metaBlock({
    'Load session': yesNo(caps.loadSession ?? caps.load_session),
    Prompt: enabledList({
      image: prompt.image,
      audio: prompt.audio,
      embeddedContext: prompt.embeddedContext ?? prompt.embedded_context,
    }),
    MCP: enabledList({ http: mcp.http, sse: mcp.sse }),
    Sessions: enabledList({
      close: sessions.close,
      fork: sessions.fork,
      list: sessions.list,
      resume: sessions.resume,
    }),
  });
}

function modelStateBlock(models) {
  const current = models.currentModelId ?? models.current_model_id;
  const available = models.availableModels ?? models.available_models ?? [];
  return listBlock(
    `Models${current ? ` / ${current}` : ''}`,
    available.map((model) => [model.name, model.modelId ?? model.model_id].filter(Boolean).join(' / ')),
  );
}

function modeStateBlock(modes) {
  const current = modes.currentModeId ?? modes.current_mode_id;
  const available = modes.availableModes ?? modes.available_modes ?? [];
  return listBlock(
    `Modes${current ? ` / ${current}` : ''}`,
    available.map((mode) => [mode.name, mode.description].filter(Boolean).join(' / ')),
  );
}

function configOptionBlocks(update) {
  const options = update.configOptions ?? update.config_options ?? [];
  if (!options.length) return [textBlock('No config options advertised.')];
  return [listBlock('Config options', options.map((option) =>
    [option.name ?? option.id, option.currentValue ?? option.current_value].filter((value) => value != null).join(': '),
  ))];
}

function planBlocks(update) {
  const entries = update.entries ?? [];
  if (!entries.length) return [jsonBlock(update, 'Plan')];
  return [{
    type: 'plan',
    entries: entries.map((entry) => ({
      content: entry.content ?? '',
      status: entry.status ?? 'pending',
      priority: entry.priority,
    })),
  }];
}

function usageBlocks(update) {
  return [metaBlock({
    Used: update.used,
    Size: update.size,
    Cost: update.cost ? `${update.cost.amount} ${update.cost.currency}` : null,
  })];
}

function commandBlocks(update) {
  const commands = update.availableCommands ?? update.available_commands ?? [];
  if (!commands.length) return [textBlock('No commands advertised.')];
  return [listBlock('Commands', commands.map((command) =>
    [command.name, command.description].filter(Boolean).join(' / '),
  ))];
}

function sessionInfoBlocks(update) {
  return [
    metaBlock({
      Title: update.title,
      Updated: update.updatedAt ?? update.updated_at,
    }),
    update._meta ? jsonBlock(update._meta, 'Metadata') : null,
  ].filter(Boolean);
}

function permissionBlocks(params = {}) {
  const tool = params.toolCall ?? params.tool_call ?? {};
  return [
    ...toolCard({ seq: 'permission' }, tool).blocks,
    listBlock('Options', (params.options ?? []).map((option) =>
      [option.name, option.optionId ?? option.option_id, option.kind].filter(Boolean).join(' / '),
    )),
  ];
}

function blocksFromResource(resource) {
  if (!resource) return [];
  if (typeof resource.text === 'string') {
    return [
      metaBlock({
        Resource: resource.uri,
        Type: resource.mimeType ?? resource.mime_type,
      }),
      textBlock(resource.text),
    ];
  }
  if (resource.blob) {
    return [{
      type: 'resource',
      title: resource.uri ?? 'Embedded resource',
      uri: resource.uri,
      mime: resource.mimeType ?? resource.mime_type,
      size: `${resource.blob.length} base64 chars`,
    }];
  }
  return [jsonBlock(resource, 'Resource')];
}

function textBlock(text) {
  return text ? { type: 'text', text } : null;
}

function metaBlock(values) {
  const entries = Object.entries(values)
    .filter(([, value]) => value != null && value !== '')
    .map(([label, value]) => ({ label, value: String(value) }));
  return entries.length ? { type: 'meta', entries } : null;
}

function listBlock(title, items) {
  const cleanItems = (items ?? []).filter(Boolean);
  return cleanItems.length ? { type: 'list', title, items: cleanItems } : null;
}

function jsonBlock(value, title = 'JSON') {
  return { type: 'json', title, value };
}

function appendBlocks(current, next) {
  const blocks = [...(current ?? [])];
  for (const block of next ?? []) {
    if (!block) continue;
    const previous = blocks.at(-1);
    if (previous?.type === 'text' && block.type === 'text') {
      previous.text = `${previous.text ?? ''}${block.text ?? ''}`;
    } else {
      blocks.push(block);
    }
  }
  return blocks;
}

function blocksToText(blocks) {
  return (blocks ?? [])
    .map((block) => {
      if (!block) return '';
      if (block.type === 'text') return block.text ?? '';
      if (block.type === 'diff') return `[Diff: ${block.path}]`;
      if (block.type === 'image') return `[Image: ${block.label ?? block.mime ?? 'image'}]`;
      if (block.type === 'audio') return `[Audio: ${block.label ?? block.mime ?? 'audio'}]`;
      if (block.type === 'resource') return `[Resource: ${block.title ?? block.uri ?? 'resource'}]`;
      if (block.type === 'terminal') return `[Terminal: ${block.terminalId}]`;
      return '';
    })
    .filter(Boolean)
    .join('\n');
}

function hasDisplayBlocks(blocks) {
  return (blocks ?? []).some(Boolean);
}

function usageSummary(usage) {
  const parts = [];
  if (usage.inputTokens ?? usage.input_tokens) parts.push(`in ${usage.inputTokens ?? usage.input_tokens}`);
  if (usage.outputTokens ?? usage.output_tokens) parts.push(`out ${usage.outputTokens ?? usage.output_tokens}`);
  if (usage.thoughtTokens ?? usage.thought_tokens) parts.push(`thought ${usage.thoughtTokens ?? usage.thought_tokens}`);
  if (usage.totalTokens ?? usage.total_tokens) parts.push(`total ${usage.totalTokens ?? usage.total_tokens}`);
  return parts.join(', ');
}

function enabledList(values) {
  const enabled = Object.entries(values)
    .filter(([, value]) => Boolean(value))
    .map(([key]) => key);
  return enabled.length ? enabled.join(', ') : 'none';
}

function yesNo(value) {
  return value ? 'yes' : 'no';
}

function dataUrl(mime, data) {
  if (typeof data === 'string' && data.startsWith('data:')) return data;
  return `data:${mime};base64,${data}`;
}
