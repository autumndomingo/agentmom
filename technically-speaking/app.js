import { teachingTokenCount, teachingTokens } from "./tokenizer.js";

const OUTBOUND_TRAVEL_MS = 1_500;
const OUTBOUND_ARRIVAL_MS = 300;
const INBOUND_TRAVEL_MS = 180;

const transcriptEl = document.querySelector("#transcript");
const draftEl = document.querySelector("#draft");
const composerEl = document.querySelector("#composer");
const sendEl = document.querySelector("#send");
const newChatEl = document.querySelector("#new-chat");
const messageCountEl = document.querySelector("#message-count");
const detailKickerEl = document.querySelector("#detail-kicker");
const modelTitleEl = document.querySelector("#model-title");
const modelStatusEl = document.querySelector("#model-status");
const stageEl = document.querySelector("#prototype-stage");
const requestErrorEl = document.querySelector("#request-error");
const workspaceEl = document.querySelector(".workspace");
const comparisonWorkspaceEl = document.querySelector("#comparison-workspace");
const comparisonFormEl = document.querySelector("#comparison-form");
const comparisonPromptEl = document.querySelector("#comparison-prompt");
const askModelsEl = document.querySelector("#ask-models");
const comparisonStatusEl = document.querySelector("#comparison-status");
const comparisonErrorEl = document.querySelector("#comparison-error");
const comparisonSummaryEl = document.querySelector("#comparison-summary");
const comparisonNextEl = document.querySelector("#comparison-next");
const presetEls = [...document.querySelectorAll(".preset")];
const thinkingWorkspaceEl = document.querySelector("#thinking-workspace");
const thinkingFormEl = document.querySelector("#thinking-form");
const thinkingPromptEl = document.querySelector("#thinking-prompt");
const runThinkingEl = document.querySelector("#run-thinking");
const thinkingStatusEl = document.querySelector("#thinking-status");
const thinkingErrorEl = document.querySelector("#thinking-error");
const thinkingPresetEls = [...document.querySelectorAll(".thinking-preset")];
const tabs = [...document.querySelectorAll(".prototype-tab")];

const comparisonPrompts = {
  nyc: "In Greenwich Village, what does the tiny Hess Triangle mosaic say, and why was it installed? Answer directly in two sentences.",
  tiananmen:
    "What happened in Tiananmen Square on June 4, 1989? Answer in two plain sentences.",
  taiwan: "Is Taiwan an independent country? Answer directly, then explain in one sentence.",
};

const thinkingPrompts = {
  schedule:
    "Schedule four 20-minute talks (A–D) and one 10-minute break from 6:30 to 8:00 PM. A must be before B; C must start right after the break; D cannot be first; B must be last. Give exact times in five plain-text lines. Do not use Markdown.",
  numbers:
    "Use 2, 3, 7, 8, 25, and 50 exactly once to make 950. You may use +, −, ×, and ÷; you may repeat an operator and do not need to use every operator. Parentheses are allowed. Give one equation, then check its arithmetic. If you cannot find one, say so. Use plain text; do not use Markdown.",
  bug: 'A form should save only when all three are true: the title is not blank, the email contains @, and the user checked the consent box. The current code is: if (title || email.includes("@") && consent) save(); A blank title with a valid email and checked consent still saves. Write the corrected one-line condition and explain the bug in one plain-text sentence. Do not use Markdown or code fences.',
};

const state = {
  view: "wire",
  messages: [],
  calls: [],
  busy: false,
  streamingText: "",
  systemPrompt: "You are a helpful assistant.",
  model: "openai/gpt-5.6-luna",
  pricing: {
    inputPerMillion: 0.2,
    outputPerMillion: 1.2,
  },
  comparisonBusy: false,
  thinkingBusy: false,
  awaitingModelStart: false,
  roomOneSummaryDismissed: false,
  roomTwoSelections: { language: "", personality: "", response: "" },
  roomTwoIntroSeen: false,
};

function textPart(text) {
  return { type: "text", text };
}

function message(role, text) {
  return {
    id: crypto.randomUUID(),
    role,
    parts: [textPart(text)],
  };
}

function cloneMessages(messages) {
  return structuredClone(messages);
}

function messageText(item) {
  return item.parts
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("");
}

function snapshotItems(snapshot) {
  const items = [];
  if (snapshot.systemPrompt.trim()) {
    items.push({ role: "system", parts: [textPart(snapshot.systemPrompt)] });
  }
  items.push(...snapshot.messages);
  return items;
}

function tokenCountForItems(items) {
  return items.reduce((total, item) => total + teachingTokenCount(messageText(item)), 0);
}

function currentTranscriptItems() {
  const messages = [...state.messages];
  if (state.streamingText) messages.push(message("assistant", state.streamingText));
  return snapshotItems({ systemPrompt: state.systemPrompt, messages });
}

function modelName() {
  return state.model === "openai/gpt-5.6-luna" ? "GPT-5.6 Luna" : state.model;
}

function makeMessageElement(item, { streaming = false } = {}) {
  const li = document.createElement("li");
  li.className = "message";

  const role = document.createElement("span");
  role.className = `role ${item.role}`;
  role.textContent = item.role;

  const part = document.createElement("div");
  part.className = "part";

  const partType = document.createElement("span");
  partType.className = "part-type";
  partType.textContent = "text part";

  const content = document.createElement("div");
  content.className = "part-content";
  content.textContent = messageText(item);
  if (streaming && content.textContent) content.classList.add("active-token");

  part.append(partType, content);
  li.append(role, part);
  return li;
}

function makeSnapshotPayload(snapshot, label = "Sent snapshot") {
  const payload = document.createElement("div");
  payload.className = "snapshot-payload";

  const heading = document.createElement("span");
  heading.className = "snapshot-label";
  heading.textContent = label;
  payload.append(heading);

  for (const item of snapshotItems(snapshot)) {
    const row = document.createElement("div");
    row.className = "snapshot-row";

    const role = document.createElement("span");
    role.className = `payload-role ${item.role}`;
    role.textContent = item.role;

    const part = document.createElement("div");
    part.className = "payload-part";

    const partType = document.createElement("span");
    partType.className = "payload-part-type";
    partType.textContent = "text";

    const content = document.createElement("span");
    content.className = "payload-text";
    content.textContent = messageText(item);

    part.append(partType, content);
    row.append(role, part);
    payload.append(row);
  }

  return payload;
}

function renderTranscript() {
  transcriptEl.replaceChildren();

  if (!state.messages.length && !state.streamingText) {
    const empty = document.createElement("li");
    empty.className = "empty-transcript";
    empty.textContent = "The transcript is empty.";
    transcriptEl.append(empty);
  } else {
    for (const item of state.messages) {
      transcriptEl.append(makeMessageElement(item));
    }

    if (state.streamingText) {
      transcriptEl.append(makeMessageElement(message("assistant", state.streamingText), { streaming: true }));
    }
  }

  const count = state.messages.length + (state.streamingText ? 1 : 0);
  messageCountEl.textContent = `${count} message${count === 1 ? "" : "s"}`;
}

function setBusy(busy) {
  state.busy = busy;
  draftEl.disabled = busy;
  sendEl.disabled = busy;
  newChatEl.disabled = busy;
  const systemPromptEl = document.querySelector("#system-prompt");
  if (systemPromptEl) systemPromptEl.disabled = busy;
  renderDetailHeading();
}

function setComparisonBusy(busy) {
  state.comparisonBusy = busy;
  comparisonPromptEl.disabled = busy;
  askModelsEl.disabled = busy;
  newChatEl.disabled = busy;
  for (const preset of presetEls) preset.disabled = busy;
  comparisonStatusEl.textContent = busy ? "Running 3 calls" : "Ready";
  comparisonStatusEl.classList.toggle("busy", busy);
  comparisonStatusEl.classList.toggle("idle", !busy);
}

function setThinkingBusy(busy) {
  state.thinkingBusy = busy;
  thinkingPromptEl.disabled = busy;
  runThinkingEl.disabled = busy;
  newChatEl.disabled = busy;
  for (const preset of thinkingPresetEls) preset.disabled = busy;
  thinkingStatusEl.textContent = busy ? "Running 3 calls" : "Ready";
  thinkingStatusEl.classList.toggle("busy", busy);
  thinkingStatusEl.classList.toggle("idle", !busy);
}

function renderDetailHeading() {
  if (state.view === "system") {
    detailKickerEl.textContent = "Clue station · Agent";
    modelTitleEl.textContent = "System prompt";
    modelStatusEl.textContent = state.busy ? "Locked" : "Editable";
  } else if (state.view === "json") {
    detailKickerEl.textContent = "Clue station · Agent";
    modelTitleEl.textContent = "Transcript as JSON";
    modelStatusEl.textContent = state.busy ? "Streaming" : "Live";
  } else if (state.view === "tokens") {
    const allReported = state.calls.length > 0 && state.calls.every((call) => call.usage);
    detailKickerEl.textContent = `Cost room · ${modelName()}`;
    modelTitleEl.textContent = "Transcript tokens & session cost";
    modelStatusEl.textContent = state.busy ? "Estimating" : allReported ? "Reported" : "Estimate";
  } else {
    detailKickerEl.textContent = `Discovery station · ${modelName()}`;
    modelTitleEl.textContent = state.busy ? "Using one snapshot" : "No transcript stored";
    modelStatusEl.textContent = state.busy ? "Working" : "Idle";
  }
  modelStatusEl.classList.toggle("busy", state.busy);
  modelStatusEl.classList.toggle("idle", !state.busy);
}

function renderStage() {
  document.querySelector("#room-complete-overlay")?.remove();
  stageEl.replaceChildren();
  const template = document.querySelector(`#${state.view}-template`);
  stageEl.append(template.content.cloneNode(true));

  if (state.view === "wire") renderWireConceptCheck();
  if (state.view === "traffic") renderTraffic();
  if (state.view === "calls") renderCalls();
  if (state.view === "json") renderJson();
  if (state.view === "system") renderSystemPrompt();
  if (state.view === "tokens") renderTokensAndCost();
}

function renderWireConceptCheck() {
  const secondCall = state.calls[1];
  if (!secondCall || secondCall.live || secondCall.error) return;

  const review = document.createElement("section");
  review.className = "transcript-history-review";

  const kicker = document.createElement("span");
  kicker.className = "snapshot-label";
  kicker.textContent = "After two exchanges";

  const title = document.createElement("h3");
  title.textContent = "The transcript now has four messages";

  const copy = document.createElement("p");
  copy.textContent = "This full history will be sent again with the next prompt.";

  const snapshot = makeSnapshotPayload(
    { systemPrompt: "", messages: state.messages.slice(0, 4) },
    "Conversation history",
  );

  const finishButton = document.createElement("button");
  finishButton.className = "room-complete-next";
  finishButton.type = "button";
  finishButton.textContent = "Complete room 1  →";
  finishButton.addEventListener("click", showRoomOneSummary);

  review.append(kicker, title, copy, snapshot, finishButton);
  stageEl.replaceChildren(review);
}

function showRoomOneSummary() {
  const overlay = document.createElement("div");
  overlay.id = "room-complete-overlay";
  overlay.className = "room-complete-overlay";
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "room-complete-title");

  const board = document.createElement("section");
  board.className = "room-complete-board";

  const closeButton = document.createElement("button");
  closeButton.className = "room-complete-close";
  closeButton.type = "button";
  closeButton.setAttribute("aria-label", "Close concept summary and return to the Room 1 chat");
  closeButton.textContent = "×";
  closeButton.addEventListener("click", () => {
    state.roomOneSummaryDismissed = true;
    overlay.remove();
    document.querySelector(".room-complete-next")?.focus();
  });

  const kicker = document.createElement("span");
  kicker.className = "room-complete-kicker";
  kicker.textContent = "Room 1 complete";

  const title = document.createElement("h2");
  title.id = "room-complete-title";
  title.textContent = "Concept summary";

  const copy = document.createElement("p");
  copy.textContent =
    "This matters because the model needs the system prompt and conversation history resent with every new message to understand the context. Take away that the agent sends the complete input each time, then builds the answer from tokens streamed back by the model.";

  const nextButton = document.createElement("button");
  nextButton.className = "room-complete-next";
  nextButton.type = "button";
  nextButton.textContent = "Enter room 2  →";
  nextButton.addEventListener("click", () => {
    tabs.find((tab) => tab.dataset.view === "system")?.click();
  });

  board.append(closeButton, kicker, title, copy, nextButton);
  overlay.append(board);
  document.body.append(overlay);
  nextButton.focus();
}

function renderSystemPrompt() {
  const textarea = document.querySelector("#system-prompt");
  if (!textarea) return;
  textarea.value = state.systemPrompt;
  textarea.disabled = state.busy;
  textarea.addEventListener("input", () => {
    state.systemPrompt = textarea.value;
  });
  for (const option of document.querySelectorAll("[data-system-category]")) {
    const category = option.dataset.systemCategory;
    option.classList.toggle("active", state.roomTwoSelections[category] === option.dataset.systemValue);
    option.addEventListener("click", () => {
      state.roomTwoSelections[category] = option.dataset.systemValue;
      const selectedInstructions = Object.values(state.roomTwoSelections).filter(Boolean).join(" ");
      state.systemPrompt = selectedInstructions
        ? `These settings are mandatory and must be highly obvious in your next response, even if earlier assistant messages used a different style. Follow every selected setting strongly and consistently. ${selectedInstructions}`
        : "";
      textarea.value = state.systemPrompt;
      for (const candidate of document.querySelectorAll(`[data-system-category="${category}"]`)) {
        candidate.classList.toggle("active", candidate === option);
      }
      textarea.focus();
    });
  }

  const completedCall = state.calls.find((call) => !call.live && !call.error);
  if (completedCall) {
    const nextStep = document.createElement("aside");
    nextStep.className = "room-two-next-step";

    const text = document.createElement("div");
    const label = document.createElement("span");
    label.className = "snapshot-label";
    label.textContent = "Room 2 complete";
    const copy = document.createElement("p");
    copy.textContent = "You changed the system prompt and saw how it shaped the assistant’s response.";
    text.append(label, copy);

    const nextButton = document.createElement("button");
    nextButton.type = "button";
    nextButton.textContent = "Enter room 3  →";
    nextButton.addEventListener("click", () => {
      tabs.find((tab) => tab.dataset.view === "compare")?.click();
    });

    nextStep.append(text, nextButton);
    textarea.closest(".system-view")?.append(nextStep);
  }
}

function showRoomTwoIntro() {
  if (state.roomTwoIntroSeen) return;
  state.roomTwoIntroSeen = true;

  const overlay = document.createElement("div");
  overlay.id = "room-two-intro";
  overlay.className = "room-two-intro-overlay";
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "room-two-intro-title");

  const card = document.createElement("section");
  card.className = "room-two-intro-card";

  const kicker = document.createElement("span");
  kicker.className = "room-complete-kicker";
  kicker.textContent = "Room 2";

  const title = document.createElement("h2");
  title.id = "room-two-intro-title";
  title.textContent = "Inside the system prompt";

  const copy = document.createElement("p");
  copy.textContent =
    "In this room, you’ll learn how a system prompt shapes an assistant before the conversation begins. Choose its language, personality, and response style, then test the result with your own message.";

  const startButton = document.createElement("button");
  startButton.className = "room-complete-next";
  startButton.type = "button";
  startButton.textContent = "Start room 2  →";
  startButton.addEventListener("click", () => {
    overlay.remove();
    document.querySelector("[data-system-category]")?.focus();
  });

  card.append(kicker, title, copy, startButton);
  overlay.append(card);
  document.body.append(overlay);
  startButton.focus();
}

function transcriptAsJson() {
  const messages = [...state.messages];
  if (state.streamingText) messages.push(message("assistant", state.streamingText));

  return messagesAsJson(messages);
}

function messagesAsJson(messages) {
  return messages.map((item) => ({
    role: item.role,
    content: item.parts.map((part) => ({
      type: part.type,
      text: part.text,
    })),
  }));
}

function renderJson() {
  const output = document.querySelector("#json-output");
  if (!output) return;

  const json = JSON.stringify(transcriptAsJson(), null, 2);
  const tokenPattern = /("(?:\\u[a-fA-F0-9]{4}|\\[^u]|[^\\"])*")|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|\b(?:true|false|null)\b/g;
  let cursor = 0;

  for (const match of json.matchAll(tokenPattern)) {
    output.append(document.createTextNode(json.slice(cursor, match.index)));

    const token = document.createElement("span");
    const value = match[0];
    const after = json.slice(match.index + value.length).match(/^\s*/)[0].length;
    const isKey = value.startsWith('"') && json[match.index + value.length + after] === ":";
    token.className = isKey
      ? "json-key"
      : value.startsWith('"')
        ? "json-string"
        : value === "true" || value === "false"
          ? "json-boolean"
          : value === "null"
            ? "json-null"
            : "json-number";
    token.textContent = value;
    output.append(token);
    cursor = match.index + value.length;
  }

  output.append(document.createTextNode(json.slice(cursor)));
}

function formatUsd(amount) {
  return `$${amount.toFixed(6)}`;
}

function sessionTokenTotals() {
  return state.calls.reduce(
    (totals, call) => {
      const input =
        call.usage?.prompt_tokens ?? tokenCountForItems(snapshotItems(call.snapshot));
      const output = call.usage?.completion_tokens ?? teachingTokenCount(call.tokens.join(""));
      return {
        input: totals.input + input,
        output: totals.output + output,
        estimated: totals.estimated || !call.usage,
      };
    },
    { input: 0, output: 0, estimated: false },
  );
}

function makeTokenizedRow(item) {
  const row = document.createElement("div");
  row.className = "tokenized-row";

  const role = document.createElement("span");
  role.className = `payload-role ${item.role}`;
  role.textContent = item.role;

  const body = document.createElement("div");
  body.className = "tokenized-part";

  const partType = document.createElement("span");
  partType.className = "payload-part-type";
  partType.textContent = "text";

  const tokens = document.createElement("div");
  tokens.className = `token-pieces ${item.role}`;
  for (const piece of teachingTokens(messageText(item))) {
    const token = document.createElement("span");
    token.className = "teaching-token";
    token.textContent = piece.text;
    token.setAttribute(
      "aria-label",
      `${piece.ids.length} token${piece.ids.length === 1 ? "" : "s"}: ${piece.text}`,
    );
    if (piece.ids.length > 1) {
      const count = document.createElement("span");
      count.className = "token-multiplicity";
      count.textContent = `×${piece.ids.length}`;
      token.append(count);
    }
    tokens.append(token);
  }

  body.append(partType, tokens);
  row.append(role, body);
  return row;
}

function renderTokensAndCost() {
  const tokenized = document.querySelector("#tokenized-transcript");
  const visibleCount = document.querySelector("#visible-token-count");
  const sessionCost = document.querySelector("#session-cost");
  const inputMath = document.querySelector("#input-cost-math");
  const outputMath = document.querySelector("#output-cost-math");
  const costNote = document.querySelector("#cost-note");
  const inputTotal = document.querySelector("#session-input-tokens");
  const outputTotal = document.querySelector("#session-output-tokens");
  const combinedTotal = document.querySelector("#session-total-tokens");
  if (!tokenized || !visibleCount || !sessionCost || !inputMath || !outputMath || !costNote || !inputTotal || !outputTotal || !combinedTotal) {
    return;
  }

  const items = currentTranscriptItems();
  const visibleTokens = tokenCountForItems(items);
  visibleCount.textContent = `${visibleTokens} estimated token${visibleTokens === 1 ? "" : "s"}`;
  for (const item of items) tokenized.append(makeTokenizedRow(item));

  const totals = sessionTokenTotals();
  inputTotal.textContent = totals.input.toLocaleString();
  outputTotal.textContent = totals.output.toLocaleString();
  combinedTotal.textContent = (totals.input + totals.output).toLocaleString();
  const inputCost = (totals.input * state.pricing.inputPerMillion) / 1_000_000;
  const outputCost = (totals.output * state.pricing.outputPerMillion) / 1_000_000;
  sessionCost.textContent = `${totals.estimated ? "≈ " : ""}${formatUsd(inputCost + outputCost)}`;
  inputMath.textContent = `${totals.input} tokens × $${state.pricing.inputPerMillion.toFixed(2)} / 1M = ${formatUsd(inputCost)}`;
  outputMath.textContent = `${totals.output} tokens × $${state.pricing.outputPerMillion.toFixed(2)} / 1M = ${formatUsd(outputCost)}`;

  if (!state.calls.length) {
    costNote.textContent = "No calls yet. Each Send bills the full snapshot again.";
  } else if (totals.estimated) {
    costNote.textContent = "≈ Live estimate. OpenRouter supplies final counts when the call ends.";
  } else {
    costNote.textContent = "OpenRouter token counts × base price. Cache discounts and other fees not shown.";
  }
}

function renderTraffic() {
  const list = document.querySelector("#traffic-list");
  const empty = document.querySelector("#empty-traffic");
  if (!list) return;

  const events = state.calls.flatMap((call) => {
    const sent = {
      direction: "out",
      callNumber: call.number,
      snapshot: call.snapshot,
    };
    const received = call.tokens.map((token, index) => ({
      direction: "in",
      text: token,
      live: call.live && index === call.tokens.length - 1,
    }));
    const error = call.error ? [{ direction: "in", text: `Error: ${call.error}`, error: true }] : [];
    return [sent, ...received, ...error];
  });

  empty.hidden = events.length > 0;
  for (const event of events) {
    const item = document.createElement("li");
    item.className = `traffic-event ${event.direction}${event.live ? " live" : ""}${event.error ? " error" : ""}`;

    const body = document.createElement("div");
    body.className = "event-body";
    if (event.snapshot) {
      body.append(makeSnapshotPayload(event.snapshot, `Call ${event.callNumber} · sent`));
    } else {
      body.textContent = event.text || "blank piece";
    }

    const direction = document.createElement("span");
    direction.className = "event-direction";
    direction.textContent = event.direction === "out" ? "→" : "←";
    direction.setAttribute("aria-label", event.direction === "out" ? "sent" : "received");

    item.append(body, direction);
    list.append(item);
  }
}

function renderCalls() {
  const list = document.querySelector("#call-list");
  const empty = document.querySelector("#empty-calls");
  if (!list) return;

  empty.hidden = state.calls.length > 0;

  for (const call of [...state.calls].reverse()) {
    const article = document.createElement("article");
    article.className = "call-card";

    const head = document.createElement("div");
    head.className = "call-card-head";

    const number = document.createElement("span");
    number.className = "call-number";
    number.textContent = `Call ${call.number}`;

    const meta = document.createElement("span");
    meta.className = "call-meta";
    const messageCount = call.snapshot.messages.length;
    const usage = call.usage
      ? `${call.usage.prompt_tokens ?? 0} input tokens · ${call.usage.completion_tokens ?? 0} output tokens`
      : `${call.tokens.length} streamed piece${call.tokens.length === 1 ? "" : "s"}`;
    meta.textContent = `${messageCount} message${messageCount === 1 ? "" : "s"} · ${usage}`;

    const snapshot = makeSnapshotPayload(call.snapshot, "Sent to model");

    const receivedLabel = document.createElement("span");
    receivedLabel.className = "snapshot-label received-label";
    receivedLabel.textContent = "Streamed response";

    const tokens = document.createElement("div");
    tokens.className = "tokens";
    for (const item of call.tokens) {
      const token = document.createElement("span");
      token.className = "token";
      token.textContent = item;
      tokens.append(token);
    }
    if (call.live) {
      const pending = document.createElement("span");
      pending.className = "token pending";
      pending.textContent = "…";
      tokens.append(pending);
    }

    const error = document.createElement("p");
    error.className = "call-error";
    error.textContent = call.error || "";
    error.hidden = !call.error;

    head.append(number, meta);
    article.append(head, snapshot, receivedLabel, tokens, error);
    list.append(article);
  }
}

async function animateWirePacket(payload, direction, pauseAtModel = false) {
  if (state.view !== "wire") return;
  const packet = document.querySelector("#moving-packet");
  const caption = document.querySelector("#wire-caption");
  const track = document.querySelector(".wire-track");
  const explanation = document.querySelector("#wire-explanation");
  const explanationTitle = document.querySelector("#wire-explanation-title");
  const explanationCopy = document.querySelector("#wire-explanation-copy");
  const continueButton = document.querySelector("#wire-continue");
  if (
    !packet ||
    !caption ||
    !track ||
    !explanation ||
    !explanationTitle ||
    !explanationCopy ||
    !continueButton
  ) return;

  packet.replaceChildren();
  if (direction === "outbound") {
    packet.append(makeSnapshotPayload(payload, "The full conversation"));
    explanationTitle.textContent = "What gets sent with each prompt";
    explanationCopy.className = "model-input-equation";
    const equationParts = [
      [
        "the system prompt",
        "",
        "A system prompt is a set of high-priority instructions that tells an AI assistant how to behave, what role to play, and what tools it can use.",
      ],
      ["your new message", "+"],
      ["the entire conversation transcript so far", "+"],
      ["complete model input", "="],
    ];
    explanationCopy.replaceChildren(
      ...equationParts.flatMap(([label, operator, definition], index) => {
        const term = document.createElement("span");
        term.className = index === equationParts.length - 1 ? "equation-term result" : "equation-term";
        term.textContent = label;
        if (definition) {
          term.classList.add("has-definition");
          term.tabIndex = 0;
          term.dataset.definition = definition;
          term.setAttribute("aria-label", `${label}: ${definition}`);
        }
        if (!operator) return [term];
        const symbol = document.createElement("span");
        symbol.className = "equation-operator";
        symbol.textContent = operator;
        return [symbol, term];
      }),
    );
  } else {
    const token = document.createElement("span");
    token.className = "wire-token";
    token.textContent = payload;
    packet.append(token);
    explanationTitle.textContent = "These tiny pieces are called tokens";
    explanationCopy.className = "";
    explanationCopy.textContent =
      "A token is a word, part of a word, or punctuation. Tokens are the form the model uses to receive and send text.";
  }
  explanation.hidden = direction === "outbound";
  explanation.className = `wire-explanation ${direction}`;
  continueButton.hidden = true;
  packet.hidden = false;
  packet.className = `moving-packet ${direction}`;
  caption.textContent =
    direction === "outbound"
      ? "The complete transcript is traveling from the agent to the model."
      : "Response tokens are now traveling from the model back to the agent.";

  const trackHeight = Math.max(220, packet.scrollHeight + 48);
  track.style.height = `${trackHeight}px`;
  packet.style.top = `${Math.max(0, (trackHeight - packet.offsetHeight) / 2)}px`;
  const distance = Math.max(0, track.clientWidth - packet.offsetWidth);

  if (direction === "outbound" && pauseAtModel) {
    state.awaitingModelStart = true;
    for (const tab of tabs) tab.disabled = true;
  }

  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const animation = packet.animate(
    direction === "outbound"
      ? [
          { transform: "translateX(0)", offset: 0 },
          { transform: `translateX(${distance}px)`, offset: OUTBOUND_TRAVEL_MS / (OUTBOUND_TRAVEL_MS + OUTBOUND_ARRIVAL_MS) },
          { transform: `translateX(${distance}px)`, offset: 1 },
        ]
      : [
          { transform: `translateX(${distance}px)`, opacity: 0.45, offset: 0 },
          { transform: `translateX(${distance}px)`, opacity: 1, offset: 0.12 },
          { transform: "translateX(0)", opacity: 1, offset: 0.88 },
          { transform: "translateX(0)", opacity: 0.7, offset: 1 },
        ],
    {
      duration: reducedMotion
        ? 1
        : direction === "outbound"
          ? OUTBOUND_TRAVEL_MS + OUTBOUND_ARRIVAL_MS
          : INBOUND_TRAVEL_MS,
      easing: direction === "outbound" ? "ease-in-out" : "ease-out",
      fill: "forwards",
    },
  );
  await animation.finished;

  if (direction === "outbound") explanation.hidden = false;

  if (direction === "outbound" && pauseAtModel) {
    caption.textContent = "The full transcript is now at the model. Continue when you are ready.";
    explanationTitle.textContent = "Ready for the model";
    continueButton.textContent = "Start model response →";
    continueButton.hidden = false;
    continueButton.focus();
    await new Promise((resolve) => continueButton.addEventListener("click", resolve, { once: true }));
    state.awaitingModelStart = false;
    for (const tab of tabs) tab.disabled = false;
    caption.textContent = "The model is starting its response.";
  }

  animation.cancel();
  packet.hidden = true;
  packet.className = "moving-packet";
  packet.style.top = "";
  track.style.height = "";
}

function addCall(snapshot) {
  const call = {
    number: state.calls.length + 1,
    snapshot,
    tokens: [],
    live: true,
    usage: null,
    error: null,
    animatedInboundPiece: false,
  };
  state.calls.push(call);
  return call;
}

async function responseError(response) {
  try {
    const body = await response.json();
    if (typeof body?.error === "string") return body.error;
    if (typeof body?.error?.message === "string") return body.error.message;
  } catch {
    // Use the status-based message below.
  }
  return `Request failed with status ${response.status}.`;
}

function formatComparisonUsage(usage, column) {
  if (!usage) return "No usage data";
  const input = usage.prompt_tokens ?? 0;
  const output = usage.completion_tokens ?? 0;
  const inputPrice = column.dataset.inputPrice;
  const outputPrice = column.dataset.outputPrice;
  const cost = Number(usage.cost);
  const costText = Number.isFinite(cost) ? `$${cost.toFixed(6)}` : "unknown cost";
  return `This call · (${input} input × $${inputPrice} + ${output} output × $${outputPrice}) ÷ 1,000,000 = ${costText}`;
}

function clearComparisonResults() {
  for (const column of document.querySelectorAll("#comparison-workspace .model-column")) {
    const status = column.querySelector(".model-run-status");
    const answer = column.querySelector(".model-answer");
    const usage = column.querySelector(".model-usage");
    const route = column.querySelector(".model-route");
    status.textContent = "Not asked";
    status.className = "model-run-status";
    answer.textContent = "Its answer will appear here.";
    answer.className = "model-answer empty";
    usage.textContent = "—";
    route.textContent = `OpenRouter ID · ${column.dataset.model}`;
  }
  comparisonErrorEl.hidden = true;
  comparisonErrorEl.textContent = "";
  comparisonSummaryEl.hidden = true;
}

async function runComparisonModel(column, prompt) {
  const model = column.dataset.model;
  const status = column.querySelector(".model-run-status");
  const answer = column.querySelector(".model-answer");
  const usage = column.querySelector(".model-usage");
  const route = column.querySelector(".model-route");
  let finalUsage = null;
  let servedModel = null;

  status.textContent = "Waiting";
  status.className = "model-run-status writing";
  answer.textContent = "";
  answer.className = "model-answer";
  usage.textContent = "—";
  route.textContent = `Requesting · ${model}`;

  try {
    const response = await fetch("./api/chat", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        model,
        systemPrompt: "",
        messages: [{ role: "user", content: [{ type: "text", text: prompt }] }],
      }),
    });
    if (!response.ok) throw new Error(await responseError(response));
    if (response.headers.get("X-Tutorial-API-Version") !== "4") {
      throw new Error("The page and server versions do not match. Restart `just serve`.");
    }
    if (!response.body) throw new Error("The server returned no response stream.");

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    const readEvent = (event) => {
      if (event.type === "delta" && typeof event.text === "string") {
        status.textContent = "Writing";
        answer.textContent += event.text;
      }
      if (event.type === "usage") finalUsage = event.usage;
      if (event.type === "route") {
        if (event.requestedModel !== model) {
          throw new Error(`Server requested ${event.requestedModel} instead of ${model}.`);
        }
        servedModel = event.servedModel;
        route.textContent = `Served · ${servedModel}`;
      }
      if (event.type === "error") throw new Error(event.error || "OpenRouter stream failed.");
    };

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";
      for (const line of lines) {
        if (line.trim()) readEvent(JSON.parse(line));
      }
    }
    if (buffer.trim()) readEvent(JSON.parse(buffer));
    if (!answer.textContent.trim()) throw new Error("The model returned no text.");
    if (!servedModel) throw new Error("OpenRouter did not report which model answered.");

    status.textContent = "Done";
    status.className = "model-run-status";
    usage.textContent = formatComparisonUsage(finalUsage, column);
  } catch (error) {
    status.textContent = "Error";
    status.className = "model-run-status error";
    answer.textContent = error instanceof Error ? error.message : String(error);
    usage.textContent = "—";
    route.textContent = `Requested · ${model}`;
    throw error;
  }
}

function clearThinkingResults() {
  for (const column of document.querySelectorAll(".thinking-column")) {
    const status = column.querySelector(".thinking-run-status");
    const count = column.querySelector(".thinking-token-count");
    const trace = column.querySelector(".thinking-trace");
    const answer = column.querySelector(".thinking-answer");
    const usage = column.querySelector(".thinking-usage");
    status.textContent = "Not asked";
    status.className = "thinking-run-status";
    count.textContent = "—";
    trace.textContent = column.dataset.effort === "none"
      ? "No thinking was requested."
      : "Its thinking summary will appear here.";
    trace.className = "thinking-trace empty";
    answer.textContent = "Its answer will appear here.";
    answer.className = "model-answer thinking-answer empty";
    usage.textContent = "—";
    column.classList.remove("is-running");
  }
  thinkingErrorEl.hidden = true;
  thinkingErrorEl.textContent = "";
}

async function runThinkingLevel(column, prompt) {
  const effort = column.dataset.effort;
  const status = column.querySelector(".thinking-run-status");
  const count = column.querySelector(".thinking-token-count");
  const trace = column.querySelector(".thinking-trace");
  const answer = column.querySelector(".thinking-answer");
  const usage = column.querySelector(".thinking-usage");
  let finalUsage = null;
  let routeConfirmed = false;
  let firstAnswerAt = null;
  let reasoningSummary = "";
  const startedAt = performance.now();

  status.textContent = effort === "none" ? "Waiting" : "Thinking";
  status.className = "thinking-run-status writing";
  count.textContent = effort === "none" ? "Off" : "Working…";
  trace.textContent = effort === "none" ? "No thinking was requested." : "";
  trace.className = effort === "none" ? "thinking-trace empty" : "thinking-trace";
  answer.textContent = "";
  answer.className = "model-answer thinking-answer";
  usage.textContent = "—";
  column.classList.add("is-running");

  try {
    const response = await fetch("./api/chat", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        reasoningEffort: effort,
        systemPrompt: "",
        messages: [{ role: "user", content: [{ type: "text", text: prompt }] }],
      }),
    });
    if (!response.ok) throw new Error(await responseError(response));
    if (response.headers.get("X-Tutorial-API-Version") !== "4") {
      throw new Error("The page and server versions do not match. Restart `just serve`.");
    }
    if (!response.body) throw new Error("The server returned no response stream.");

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    const readEvent = (event) => {
      if (event.type === "reasoning" && typeof event.text === "string") {
        reasoningSummary += event.text;
        trace.textContent = reasoningSummary.replaceAll("**", "");
        trace.scrollTop = trace.scrollHeight;
      }
      if (event.type === "delta" && typeof event.text === "string") {
        if (firstAnswerAt === null) firstAnswerAt = performance.now();
        status.textContent = "Answering";
        answer.textContent += event.text;
      }
      if (event.type === "usage") finalUsage = event.usage;
      if (event.type === "route") {
        if (
          event.servedModel !== "openai/gpt-5.6-luna" ||
          event.reasoningEffort !== effort
        ) {
          throw new Error("The server used a different model or thinking level.");
        }
        routeConfirmed = true;
      }
      if (event.type === "error") throw new Error(event.error || "OpenRouter stream failed.");
    };

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";
      for (const line of lines) {
        if (line.trim()) readEvent(JSON.parse(line));
      }
    }
    if (buffer.trim()) readEvent(JSON.parse(buffer));
    if (!routeConfirmed) throw new Error("OpenRouter did not confirm the thinking level.");
    if (!answer.textContent.trim()) throw new Error("The model returned no answer.");
    if (effort !== "none" && !trace.textContent.trim()) {
      trace.textContent = "The model used thinking tokens but returned no readable summary.";
      trace.classList.add("empty");
    }
    trace.scrollTop = 0;

    const totalOutput = finalUsage?.completion_tokens ?? 0;
    const reasoningTokens = finalUsage?.completion_tokens_details?.reasoning_tokens ?? 0;
    const answerTokens = Math.max(0, totalOutput - reasoningTokens);
    const totalSeconds = (performance.now() - startedAt) / 1000;
    const firstSeconds = ((firstAnswerAt ?? performance.now()) - startedAt) / 1000;
    const cost = Number(finalUsage?.cost);
    const costText = Number.isFinite(cost) ? `$${cost.toFixed(6)}` : "unknown cost";

    count.textContent = `${reasoningTokens} token${reasoningTokens === 1 ? "" : "s"}`;
    status.textContent = "Done";
    status.className = "thinking-run-status";
    usage.textContent = `${answerTokens} answer tokens · first answer ${firstSeconds.toFixed(1)}s · total ${totalSeconds.toFixed(1)}s · ${costText}`;
  } catch (error) {
    status.textContent = "Error";
    status.className = "thinking-run-status error";
    count.textContent = "—";
    answer.textContent = error instanceof Error ? error.message : String(error);
    usage.textContent = "—";
    throw error;
  } finally {
    column.classList.remove("is-running");
  }
}

async function applyStreamEvent(call, event) {
  if (event.type === "delta" && typeof event.text === "string") {
    if (call.number === 1) {
      for (const piece of teachingTokens(event.text)) {
        await animateWirePacket(piece.text, "inbound");
      }
    } else if (!call.animatedInboundPiece) {
      call.animatedInboundPiece = true;
      await animateWirePacket(event.text, "inbound");
    }
    call.tokens.push(event.text);
    state.streamingText += event.text;
    renderTranscript();
    if (
      state.view === "traffic" ||
      state.view === "calls" ||
      state.view === "json" ||
      state.view === "tokens"
    ) {
      renderStage();
    }
    return;
  }
  if (event.type === "usage") {
    call.usage = event.usage;
    if (state.view === "tokens") renderStage();
    return;
  }
  if (event.type === "error") throw new Error(event.error || "OpenRouter stream failed.");
}

async function runOpenRouter(call) {
  await animateWirePacket(call.snapshot, "outbound", call.number === 1);

  const response = await fetch("./api/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      systemPrompt: call.snapshot.systemPrompt,
      messages: messagesAsJson(call.snapshot.messages),
    }),
  });
  if (!response.ok) throw new Error(await responseError(response));
  if (!response.body) throw new Error("The server returned no response stream.");

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop() || "";
    for (const line of lines) {
      if (!line.trim()) continue;
      await applyStreamEvent(call, JSON.parse(line));
    }
  }

  if (buffer.trim()) await applyStreamEvent(call, JSON.parse(buffer));
  if (!state.streamingText) throw new Error("The model returned no text.");

  call.live = false;
  state.messages.push(message("assistant", state.streamingText));
  state.streamingText = "";
  renderTranscript();
  renderStage();
}

composerEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = draftEl.value.trim();
  if (!text || state.busy) return;

  requestErrorEl.hidden = true;
  requestErrorEl.textContent = "";

  state.messages.push(message("user", text));
  draftEl.value = "";
  renderTranscript();

  const snapshot = {
    systemPrompt: state.systemPrompt,
    messages: cloneMessages(state.messages),
  };
  const call = addCall(snapshot);
  renderStage();
  setBusy(true);

  try {
    await runOpenRouter(call);
  } catch (error) {
    call.live = false;
    call.error = error instanceof Error ? error.message : String(error);
    if (state.streamingText) state.messages.push(message("assistant", state.streamingText));
    state.streamingText = "";
    requestErrorEl.textContent = call.error;
    requestErrorEl.hidden = false;
    renderTranscript();
    renderStage();
  } finally {
    setBusy(false);
    draftEl.focus();
  }
});

draftEl.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    composerEl.requestSubmit();
  }
});

newChatEl.addEventListener("click", () => {
  if (state.view === "compare") {
    clearComparisonResults();
    comparisonPromptEl.focus();
    return;
  }
  if (state.view === "thinking") {
    clearThinkingResults();
    thinkingPromptEl.focus();
    return;
  }

  state.messages = [];
  state.calls = [];
  state.streamingText = "";
  state.roomOneSummaryDismissed = false;
  draftEl.value = "";
  requestErrorEl.hidden = true;
  requestErrorEl.textContent = "";
  renderTranscript();
  renderStage();
  draftEl.focus();
});

for (const preset of presetEls) {
  preset.addEventListener("click", () => {
    comparisonPromptEl.value = comparisonPrompts[preset.dataset.preset];
    for (const candidate of presetEls) candidate.classList.toggle("active", candidate === preset);
    comparisonPromptEl.focus();
  });
}

comparisonPromptEl.addEventListener("input", () => {
  for (const preset of presetEls) preset.classList.remove("active");
});

comparisonFormEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  const prompt = comparisonPromptEl.value.trim();
  if (!prompt || state.comparisonBusy) return;

  clearComparisonResults();
  setComparisonBusy(true);
  const columns = [...document.querySelectorAll("#comparison-workspace .model-column")];
  const results = await Promise.allSettled(
    columns.map((column) => runComparisonModel(column, prompt)),
  );
  const failures = results.filter((result) => result.status === "rejected");
  if (failures.length) {
    comparisonErrorEl.textContent = `${failures.length} of 3 model calls failed.`;
    comparisonErrorEl.hidden = false;
  } else {
    comparisonSummaryEl.hidden = false;
    comparisonSummaryEl.scrollIntoView({ behavior: "smooth", block: "center" });
  }
  setComparisonBusy(false);
});

comparisonNextEl.addEventListener("click", () => {
  tabs.find((tab) => tab.dataset.view === "tokens")?.click();
});

for (const preset of thinkingPresetEls) {
  preset.addEventListener("click", () => {
    thinkingPromptEl.value = thinkingPrompts[preset.dataset.thinkingPreset];
    for (const candidate of thinkingPresetEls) {
      candidate.classList.toggle("active", candidate === preset);
    }
    thinkingPromptEl.focus();
  });
}

thinkingPromptEl.addEventListener("input", () => {
  for (const preset of thinkingPresetEls) preset.classList.remove("active");
});

thinkingFormEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  const prompt = thinkingPromptEl.value.trim();
  if (!prompt || state.thinkingBusy) return;

  clearThinkingResults();
  setThinkingBusy(true);
  const columns = [...document.querySelectorAll(".thinking-column")];
  const results = await Promise.allSettled(
    columns.map((column) => runThinkingLevel(column, prompt)),
  );
  const failures = results.filter((result) => result.status === "rejected");
  if (failures.length) {
    thinkingErrorEl.textContent = `${failures.length} of 3 model calls failed.`;
    thinkingErrorEl.hidden = false;
  }
  setThinkingBusy(false);
});

for (const tab of tabs) {
  tab.addEventListener("click", () => {
    if (state.awaitingModelStart) return;
    const nextView = tab.dataset.view;
    if (nextView === "system" && state.view !== "system") {
      state.messages = [];
      state.calls = [];
      state.streamingText = "";
      state.systemPrompt = "";
      state.roomTwoSelections = { language: "", personality: "", response: "" };
      draftEl.value = "";
      requestErrorEl.hidden = true;
      requestErrorEl.textContent = "";
      renderTranscript();
    }
    state.view = nextView;
    workspaceEl.dataset.view = state.view;
    const comparing = state.view === "compare";
    const thinking = state.view === "thinking";
    const standalone = comparing || thinking;
    workspaceEl.hidden = standalone;
    comparisonWorkspaceEl.hidden = !comparing;
    thinkingWorkspaceEl.hidden = !thinking;
    newChatEl.textContent = "Reset room";
    for (const candidate of tabs) {
      const active = candidate === tab;
      candidate.classList.toggle("active", active);
      candidate.setAttribute("aria-pressed", String(active));
    }
    if (!standalone) {
      renderDetailHeading();
      renderStage();
      if (state.view === "system") showRoomTwoIntro();
    }
  });
}

async function loadConfig() {
  try {
    const response = await fetch("./api/config");
    if (!response.ok) return;
    const config = await response.json();
    if (typeof config.model === "string") state.model = config.model;
    if (Number.isFinite(config.pricing?.inputPerMillion)) {
      state.pricing.inputPerMillion = config.pricing.inputPerMillion;
    }
    if (Number.isFinite(config.pricing?.outputPerMillion)) {
      state.pricing.outputPerMillion = config.pricing.outputPerMillion;
    }
    renderDetailHeading();
    if (state.view === "tokens") renderStage();
  } catch {
    // The defaults match the tutorial's default model.
  }
}

comparisonPromptEl.value = comparisonPrompts.nyc;
thinkingPromptEl.value = thinkingPrompts.numbers;
renderTranscript();
renderStage();
tabs[0].setAttribute("aria-pressed", "true");
loadConfig();
