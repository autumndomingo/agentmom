import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = mkdtempSync(join(tmpdir(), "agentmom-technically-speaking-"));
const receivedRequests: Array<Record<string, any>> = [];

const upstream = createServer(async (req, res) => {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  receivedRequests.push(request);

  res.writeHead(200, { "Content-Type": "text/event-stream" });
  if (request.reasoning?.exclude === false) {
    res.write(
      `data: ${JSON.stringify({ model: request.model, choices: [{ delta: { reasoning: "Checked the constraints. " } }] })}\n\n`
    );
  }
  res.write(`data: ${JSON.stringify({ model: request.model, choices: [{ delta: { content: "Hello" } }] })}\n\n`);
  res.write(
    'data: {"choices":[{"delta":{"content":" there"}}],"usage":{"prompt_tokens":12,"completion_tokens":2,"total_tokens":14}}\n\n'
  );
  res.end("data: [DONE]\n\n");
});

await new Promise<void>((resolve) => upstream.listen(0, "127.0.0.1", resolve));
const upstreamAddress = upstream.address();
assert(upstreamAddress && typeof upstreamAddress === "object");

const probe = createServer();
await new Promise<void>((resolve) => probe.listen(0, "127.0.0.1", resolve));
const probeAddress = probe.address();
assert(probeAddress && typeof probeAddress === "object");
const port = probeAddress.port;
await new Promise<void>((resolve, reject) => probe.close((error) => (error ? reject(error) : resolve())));

const server = spawn(process.execPath, ["node_modules/tsx/dist/cli.mjs", "src/server.ts"], {
  cwd: new URL("..", import.meta.url),
  env: {
    ...process.env,
    AGENTMOM_AUTH_ENABLED: "1",
    AGENTMOM_DEV_AUTH_PASSWORD: "password",
    AGENTMOM_DEV_AUTH_USERS: "user@example.com|Demo User|user",
    AGENTMOM_PORT: String(port),
    AGENTMOM_STATE_DIR: join(root, "state"),
    AGENTMOM_WORKSPACE: join(root, "workspace"),
    AGENTMOM_WORKSPACE_ROOT: join(root, "workspaces"),
    AGENTMOM_PROJECTS_DIR: join(root, "projects"),
    AGENTMOM_TELEGRAM_BOT_TOKEN: "smoke-telegram-token",
    AGENTMOM_TELEGRAM_DISABLED: "1",
    BRAVE_API_KEY: "smoke-brave-key",
    OPENROUTER_API_KEY: "smoke-openrouter-key",
    OPENROUTER_CHAT_URL: `http://127.0.0.1:${upstreamAddress.port}`
  },
  stdio: "ignore"
});

const baseUrl = `http://127.0.0.1:${port}`;

try {
  let ready = false;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/api/health`);
      if (response.ok) {
        ready = true;
        break;
      }
    } catch {
      // Server still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  assert.equal(ready, true, "Agent Mom did not start");

  const anonymousPage = await fetch(`${baseUrl}/technically-speaking/`, { redirect: "manual" });
  assert.equal(anonymousPage.status, 302);
  assert.equal(anonymousPage.headers.get("location"), "/");

  const anonymousChat = await fetch(`${baseUrl}/technically-speaking/api/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ systemPrompt: "", messages: [] })
  });
  assert.equal(anonymousChat.status, 401);
  assert.equal(receivedRequests.length, 0, "anonymous request reached OpenRouter");

  const login = await fetch(`${baseUrl}/api/auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email: "user@example.com", password: "password" })
  });
  assert.equal(login.status, 200);
  const cookie = login.headers.get("set-cookie")?.split(";", 1)[0];
  assert(cookie);
  const headers = { Cookie: cookie };

  const redirect = await fetch(`${baseUrl}/technically-speaking`, { headers, redirect: "manual" });
  assert.equal(redirect.status, 302);
  assert.equal(redirect.headers.get("location"), "/technically-speaking/");

  const page = await fetch(`${baseUrl}/technically-speaking/`, { headers });
  assert.equal(page.status, 200);
  assert.match(await page.text(), /The Agent Escape Room/);

  const config = await fetch(`${baseUrl}/technically-speaking/api/config`, { headers });
  assert.equal(config.status, 200);
  assert.deepEqual(await config.json(), {
    model: "openai/gpt-5.6-luna",
    pricing: { inputPerMillion: 0.2, outputPerMillion: 1.2 }
  });

  const chat = await fetch(`${baseUrl}/technically-speaking/api/chat`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({
      systemPrompt: "Answer like a pirate.",
      messages: [{ role: "user", content: [{ type: "text", text: "Hello" }] }]
    })
  });
  assert.equal(chat.status, 200);
  assert.equal(chat.headers.get("x-tutorial-api-version"), "4");
  const events = (await chat.text())
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  assert.deepEqual(
    events.filter((event) => event.type === "delta").map((event) => event.text),
    ["Hello", " there"]
  );
  assert.equal(events.find((event) => event.type === "usage").usage.total_tokens, 14);
  assert.equal(receivedRequests[0].model, "openai/gpt-5.6-luna");
  assert.deepEqual(receivedRequests[0].reasoning, { effort: "none", exclude: true });

  const comparison = await fetch(`${baseUrl}/technically-speaking/api/chat`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({
      model: "meta-llama/llama-3.2-1b-instruct",
      systemPrompt: "",
      messages: [{ role: "user", content: [{ type: "text", text: "Hello" }] }]
    })
  });
  assert.equal(comparison.status, 200);
  await comparison.text();
  assert.equal(receivedRequests[1].model, "meta-llama/llama-3.2-1b-instruct");
  assert.equal(receivedRequests[1].reasoning, undefined);

  const thinking = await fetch(`${baseUrl}/technically-speaking/api/chat`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({
      reasoningEffort: "high",
      systemPrompt: "",
      messages: [{ role: "user", content: [{ type: "text", text: "Solve this" }] }]
    })
  });
  assert.equal(thinking.status, 200);
  const thinkingEvents = (await thinking.text())
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  assert.deepEqual(receivedRequests[2].reasoning, { effort: "high", exclude: false });
  assert.equal(receivedRequests[2].max_completion_tokens, 4096);
  assert.equal(thinkingEvents.find((event) => event.type === "reasoning").text, "Checked the constraints. ");

  const rejected = await fetch(`${baseUrl}/technically-speaking/api/chat`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ model: "unapproved/model", systemPrompt: "", messages: [] })
  });
  assert.equal(rejected.status, 400);

  console.log("technically-speaking smoke ok");
} finally {
  server.kill("SIGTERM");
  await new Promise<void>((resolve) => upstream.close(() => resolve()));
  rmSync(root, { recursive: true, force: true });
}
