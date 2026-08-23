import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { readFileSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";
import { createServer as createViteServer, type ViteDevServer } from "vite";
import {
  CatalogStore,
  SESSION_COOKIE,
  clearSessionCookie,
  sessionCookie,
  type CatalogUser,
  type CatalogWorkspace
} from "./catalog.js";
import { loadConfig } from "./config.js";
import { seedDevAuthUsers } from "./dev-auth.js";
import { deploymentPath, deploymentSlugFromHost } from "./deployment-routing.js";
import { DeploymentManager } from "./deployments.js";
import {
  deploymentRequestHeaders,
  errorStatus,
  isAddressInUse,
  isSecureRequest,
  previewRequestHeaders,
  readBody,
  readJson,
  sendError,
  sendJson,
  sendProxyResponse,
  serveStatic
} from "./http-utils.js";
import { startSandboxReaper } from "./sandbox-reaper.js";
import { ensureWorkspaceProjectPath, workspaceApiPath, workspacePreviewPath } from "./server-paths.js";
import { createSkill, deleteSkill, listSkillFiles, readSkillFile, writeSkillFile } from "./skills.js";
import { handleTechnicallySpeakingApi } from "./technically-speaking.js";
import { TelegramChannel } from "./telegram-channel.js";
import {
  MAX_MESSAGE_ATTACHMENT_BYTES,
  MAX_MESSAGE_ATTACHMENT_TOTAL_BYTES,
  MAX_MESSAGE_ATTACHMENTS,
  type AppState,
  type MessageAttachment
} from "./types.js";
import { WorkspaceRuntimeManager } from "./workspace-runtime.js";

const config = loadConfig({ requireServiceSecrets: true });
const catalog = new CatalogStore(config);
const deployments = new DeploymentManager(config);
const runtimes = new WorkspaceRuntimeManager(config, deployments);
const isProduction = process.env.NODE_ENV === "production";
let telegram: TelegramChannel | undefined;

seedDevAuthUsers(catalog, config.authEnabled, isProduction);

let vite: ViteDevServer | undefined;

const server = createServer(async (req, res) => {
  try {
    const url = new URL(req.url ?? "/", `http://${req.headers.host ?? `${config.host}:${config.port}`}`);

    if (url.pathname === "/api/tls-ask" && req.method === "GET") {
      const domain = url.searchParams.get("domain") ?? undefined;
      const slug = deploymentSlugFromHost(domain, config.deploymentBaseDomain);
      if (slug && (await deployments.isRoutable(slug))) {
        res.writeHead(200);
        res.end("ok");
      } else {
        res.writeHead(403);
        res.end("forbidden");
      }
      return;
    }

    const deploymentSlug = deploymentSlugFromHost(req.headers.host, config.deploymentBaseDomain);
    if (deploymentSlug) {
      const body = await readBody(req);
      const response = await deployments.fetch(
        deploymentSlug,
        {
          method: req.method ?? "GET",
          path: `${url.pathname}${url.search}`,
          headers: deploymentRequestHeaders(req),
          body: body.length > 0 ? body : undefined
        },
        "host"
      );
      return sendProxyResponse(res, response.status, response.headers, req.method === "HEAD" ? undefined : response.body);
    }

    const deployment = deploymentPath(url.pathname);
    if (deployment && config.deploymentBaseDomain) {
      res.writeHead(302, {
        Location: `https://${deployment.slug}.${config.deploymentBaseDomain}${deployment.upstreamPath}${url.search}`
      });
      res.end();
      return;
    }

    if (url.pathname.startsWith("/deploy/") && !url.pathname.slice("/deploy/".length).includes("/")) {
      res.writeHead(302, { Location: `${url.pathname}/${url.search}` });
      res.end();
      return;
    }

    if (deployment) {
      const body = await readBody(req);
      const response = await deployments.fetch(deployment.slug, {
        method: req.method ?? "GET",
        path: `${deployment.upstreamPath}${url.search}`,
        headers: deploymentRequestHeaders(req),
        body: body.length > 0 ? body : undefined
      });
      return sendProxyResponse(res, response.status, response.headers, req.method === "HEAD" ? undefined : response.body);
    }

    const tutorialApi = url.pathname.startsWith("/technically-speaking/api/");
    const tutorialPage =
      url.pathname === "/technically-speaking" || url.pathname.startsWith("/technically-speaking/");
    if (tutorialPage) {
      if (!currentUser(req)) {
        if (tutorialApi) return sendError(res, new Error("unauthorized"), 401);
        res.writeHead(302, { Location: "/" });
        res.end();
        return;
      }
      if (url.pathname === "/technically-speaking") {
        res.writeHead(302, { Location: `/technically-speaking/${url.search}` });
        res.end();
        return;
      }
      if (tutorialApi) {
        return await handleTechnicallySpeakingApi(url.pathname, req, res, config);
      }
    }

    const preview = workspacePreviewPath(url.pathname);
    if (preview?.needsSlash) {
      res.writeHead(302, { Location: `${url.pathname}/${url.search}` });
      res.end();
      return;
    }
    if (preview) {
      const workspace = authorizeWorkspace(req, preview.workspaceId);
      const { bridge } = await runtimes.get(workspace);
      const body = await readBody(req);
      const response = await bridge.fetchPreview(preview.previewId, {
        method: req.method ?? "GET",
        path: `${preview.upstreamPath}${url.search}`,
        headers: previewRequestHeaders(req),
        body: body.length > 0 ? body : undefined
      });
      return sendProxyResponse(res, response.status, response.headers, req.method === "HEAD" ? undefined : response.body);
    }

    if (url.pathname === "/api/sandbox-shim" && req.method === "GET") {
      res.writeHead(200, { "Content-Type": "text/javascript; charset=utf-8" });
      res.end(readFileSync(resolve(config.rootDir, "src", "sandbox-shim.mjs"), "utf8"));
      return;
    }

    if (url.pathname === "/api/health") {
      return sendJson(res, {
        ok: true,
        commit: config.appCommit,
        authEnabled: config.authEnabled,
        telegramEnabled: Boolean(telegram),
        workspaceRoot: config.workspaceRoot,
        executor: config.executor
      });
    }

    if (url.pathname === "/api/me" && req.method === "GET") {
      const user = currentUser(req);
      if (!user) return sendJson(res, { ok: false, authEnabled: config.authEnabled, error: "unauthorized" }, 401);
      return sendJson(res, { ok: true, authEnabled: config.authEnabled, ...catalog.me(user) });
    }

    if (url.pathname === "/api/auth/signup" && req.method === "POST") {
      const body = await readJson(req);
      const result = catalog.signup({
        email: String(body?.email ?? ""),
        fullName: String(body?.fullName ?? ""),
        password: String(body?.password ?? ""),
        inviteCode: typeof body?.inviteCode === "string" ? body.inviteCode : undefined
      });
      return sendJson(
        res,
        { ok: true, authEnabled: config.authEnabled, ...catalog.me(catalog.currentUser(`${SESSION_COOKIE}=${result.token}`)!) },
        200,
        { "Set-Cookie": sessionCookie(result.token, isSecureRequest(req)) }
      );
    }

    if (url.pathname === "/api/auth/login" && req.method === "POST") {
      const body = await readJson(req);
      const result = catalog.login({
        email: String(body?.email ?? ""),
        password: String(body?.password ?? "")
      });
      return sendJson(
        res,
        { ok: true, authEnabled: config.authEnabled, ...catalog.me(catalog.currentUser(`${SESSION_COOKIE}=${result.token}`)!) },
        200,
        { "Set-Cookie": sessionCookie(result.token, isSecureRequest(req)) }
      );
    }

    if (url.pathname === "/api/auth/logout" && req.method === "POST") {
      catalog.logout(req.headers.cookie);
      return sendJson(res, { ok: true }, 200, { "Set-Cookie": clearSessionCookie() });
    }

    if (url.pathname === "/api/admin/invites" && req.method === "GET") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, invites: catalog.invites(user) });
    }

    if (url.pathname === "/api/admin/users" && req.method === "GET") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, users: catalog.users(user) });
    }

    const setUserRole = /^\/api\/admin\/users\/([^/]+)\/role$/.exec(url.pathname);
    if (setUserRole && req.method === "POST") {
      const user = requireUser(req);
      const body = await readJson(req);
      const role = typeof body?.role === "string" ? body.role : "user";
      return sendJson(res, { ok: true, user: catalog.setUserRole(user, decodeURIComponent(setUserRole[1]), role) });
    }

    if (url.pathname === "/api/admin/invites" && req.method === "POST") {
      const user = requireUser(req);
      const body = await readJson(req);
      const result = catalog.createInvite(user, {
        label: typeof body?.label === "string" ? body.label : undefined,
        role: typeof body?.role === "string" ? body.role : undefined
      });
      return sendJson(res, { ok: true, invite: result.invite, code: result.code });
    }

    const disableInvite = /^\/api\/admin\/invites\/([^/]+)\/disable$/.exec(url.pathname);
    if (disableInvite && req.method === "POST") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, invite: catalog.disableInvite(user, decodeURIComponent(disableInvite[1])) });
    }

    if (url.pathname === "/api/admin/access-code" && req.method === "GET") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, accessCode: catalog.accessCode(user) });
    }

    if (url.pathname === "/api/admin/access-code/regenerate" && req.method === "POST") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, accessCode: catalog.regenerateAccessCode(user) });
    }

    if (url.pathname === "/api/workspaces" && req.method === "GET") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, workspaces: catalog.me(user).workspaces });
    }

    if (url.pathname === "/api/telegram" && req.method === "GET") {
      const user = requireUser(req);
      const code = catalog.currentTelegramLinkCode(user);
      return sendJson(res, {
        ok: true,
        enabled: Boolean(config.telegram.botToken),
        botUsername: telegram?.username(),
        linkCode: code
          ? {
              code: code.code,
              command: `/link ${code.code}`,
              expiresAt: code.expiresAt,
              botUsername: telegram?.username()
            }
          : undefined,
        links: catalog.telegramLinks(user)
      });
    }

    if (url.pathname === "/api/telegram/link-code" && req.method === "POST") {
      const user = requireUser(req);
      if (!config.telegram.botToken) throw new Error("telegram bot is not configured");
      const code = catalog.createTelegramLinkCode(user);
      return sendJson(res, {
        ok: true,
        code: code.code,
        command: `/link ${code.code}`,
        expiresAt: code.expiresAt,
        botUsername: telegram?.username()
      });
    }

    const telegramUnlink = /^\/api\/telegram\/links\/([^/]+)$/.exec(url.pathname);
    if (telegramUnlink && req.method === "DELETE") {
      const user = requireUser(req);
      return sendJson(res, { ok: true, link: catalog.unlinkTelegram(user, decodeURIComponent(telegramUnlink[1])) });
    }

    const workspaceRoute = workspaceApiPath(url.pathname);
    if (workspaceRoute) {
      const workspace = authorizeWorkspace(req, workspaceRoute.workspaceId);
      const runtime = await runtimes.get(workspace);
      return await handleWorkspaceRoute(runtime, workspaceRoute.rest, url, req, res);
    }

    if (vite) {
      return vite.middlewares(req, res, (error?: unknown) => {
        if (error) {
          vite?.ssrFixStacktrace(error as Error);
          sendError(res, error);
        } else {
          sendError(res, new Error("Not found"), 404);
        }
      });
    }

    return serveStatic(config.rootDir, url.pathname, res);
  } catch (error) {
    sendError(res, error, errorStatus(error));
  }
});

if (!isProduction) {
  vite = await createViteServer({
    server: { middlewareMode: true, hmr: { server } },
    appType: "spa"
  });
}

await startServer();
startTelegramChannel();
startSandboxReaper(config, catalog, runtimes);

process.on("SIGINT", () => shutdown());
process.on("SIGTERM", () => shutdown());

async function handleWorkspaceRoute(
  runtime: Awaited<ReturnType<WorkspaceRuntimeManager["get"]>>,
  rest: string,
  url: URL,
  req: IncomingMessage,
  res: ServerResponse
): Promise<void> {
  const { bridge } = runtime;

  if (rest === "/state" && req.method === "GET") {
    return sendJson(res, await bridge.snapshot());
  }

  if (rest === "/sessions" && req.method === "GET") {
    return sendJson(res, { sessions: await bridge.listSessions() });
  }

  if (rest === "/sessions" && req.method === "POST") {
    const body = await readJson(req);
    const state = await bridge.openSession(
      body?.path ? { kind: "open", path: String(body.path) } : body?.new ? { kind: "new" } : { kind: "continue" }
    );
    return sendJson(res, state);
  }

  if (rest === "/previews" && req.method === "GET") {
    return sendJson(res, { previews: bridge.listPreviews() });
  }

  if (rest === "/previews" && req.method === "POST") {
    const body = await readJson(req);
    const port = Number.parseInt(String(body?.port ?? ""), 10);
    const name = typeof body?.name === "string" ? body.name.trim() : "";
    if (!name) {
      return sendJson(res, { error: "name is required" }, 400);
    }
    return sendJson(res, bridge.registerPreview(port, name));
  }

  if (rest === "/deployments" && req.method === "GET") {
    return sendJson(res, {
      deployments: await deployments.list({
        workspaceId: runtime.config.workspaceId,
        workspaceDirName: runtime.config.workspaceDirName
      })
    });
  }

  if (rest === "/deployments" && req.method === "POST") {
    const body = await readJson(req);
    if (!body?.path || typeof body.path !== "string") {
      return sendJson(res, { error: "path is required" }, 400);
    }
    const rawPath = body.path.trim();
    const projectPath = isAbsolute(rawPath) ? rawPath : resolve(runtime.config.agentCwd, rawPath);
    try {
      const scopedProjectPath = ensureWorkspaceProjectPath(projectPath, runtime.config.projectsDir);
      return sendJson(
        res,
        await deployments.publish({
          path: scopedProjectPath,
          slug: body?.slug ? String(body.slug) : undefined,
          port: body?.port === undefined || body?.port === "" ? undefined : Number.parseInt(String(body.port), 10),
          staticDir: typeof body?.static === "string" ? body.static : undefined,
          workspaceId: runtime.config.workspaceId,
          workspaceDirName: runtime.config.workspaceDirName
        })
      );
    } catch (error) {
      return sendError(res, error, 400);
    }
  }

  if (rest.startsWith("/deployments/") && rest.endsWith("/logs") && req.method === "GET") {
    const slug = decodeURIComponent(rest.slice("/deployments/".length, -"/logs".length));
    const tail = Number.parseInt(url.searchParams.get("tail") ?? "200", 10);
    return sendJson(res, {
      logs: await deployments.logs(slug, tail, {
        workspaceId: runtime.config.workspaceId,
        workspaceDirName: runtime.config.workspaceDirName
      })
    });
  }

  if (rest.startsWith("/deployments/") && req.method === "DELETE") {
    const slug = decodeURIComponent(rest.slice("/deployments/".length));
    return sendJson(res, {
      removed: await deployments.remove(slug, {
        workspaceId: runtime.config.workspaceId,
        workspaceDirName: runtime.config.workspaceDirName
      }),
      deployments: await deployments.list({
        workspaceId: runtime.config.workspaceId,
        workspaceDirName: runtime.config.workspaceDirName
      })
    });
  }

  const previewDelete = /^\/previews\/([^/]+)$/.exec(rest);
  if (previewDelete && req.method === "DELETE") {
    return sendJson(res, bridge.removePreview(decodeURIComponent(previewDelete[1])));
  }

  if (rest === "/skills" && req.method === "GET") {
    return sendJson(res, { skills: bridge.listSkills() });
  }

  if (rest === "/skills" && req.method === "POST") {
    const body = await readJson(req);
    createSkill(runtime.config, String(body?.name ?? ""));
    await bridge.syncProjectSkillsToSandbox();
    return sendJson(res, await bridge.refreshSkills());
  }

  if (rest === "/skills" && req.method === "DELETE") {
    const baseDir = url.searchParams.get("baseDir") ?? "";
    deleteSkill(runtime.config, baseDir);
    await bridge.syncProjectSkillsToSandbox();
    return sendJson(res, await bridge.refreshSkills());
  }

  if (rest === "/skills/files" && req.method === "GET") {
    const baseDir = url.searchParams.get("baseDir") ?? "";
    return sendJson(res, { files: listSkillFiles(runtime.config, baseDir) });
  }

  if (rest === "/skills/file" && req.method === "GET") {
    const path = url.searchParams.get("path") ?? "";
    return sendJson(res, readSkillFile(runtime.config, path));
  }

  if (rest === "/skills/file" && req.method === "PUT") {
    const body = await readJson(req);
    if (typeof body?.path !== "string" || typeof body?.content !== "string") {
      return sendJson(res, { error: "path and content are required" }, 400);
    }
    writeSkillFile(runtime.config, body.path, body.content);
    await bridge.syncProjectSkillsToSandbox();
    return sendJson(res, await bridge.refreshSkills());
  }

  if (rest === "/messages" && req.method === "POST") {
    const body = await readJson(req);
    const message = parseMessageRequest(body);
    if (!message.content.trim() && message.attachments.length === 0) {
      return sendJson(res, { error: "content or attachment is required" }, 400);
    }
    return sendJson(res, await bridge.sendMessage(message));
  }

  if (rest === "/cancel" && req.method === "POST") {
    return sendJson(res, await bridge.cancel());
  }

  if (rest === "/events" && req.method === "GET") {
    return handleSse(bridge, res);
  }

  return sendError(res, new Error(`Not found: ${url.pathname}`), 404);
}

function handleSse(
  bridge: Awaited<ReturnType<WorkspaceRuntimeManager["get"]>>["bridge"],
  res: ServerResponse
): void {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache, no-transform",
    Connection: "keep-alive",
    "X-Accel-Buffering": "no"
  });

  const send = (state: AppState) => {
    res.write(`event: state\n`);
    res.write(`data: ${JSON.stringify(state)}\n\n`);
  };

  const unsubscribe = bridge.subscribe(send);
  res.on("close", unsubscribe);
}

function currentUser(req: IncomingMessage): CatalogUser | null {
  if (!config.authEnabled) return catalog.ensureDevUser().user;
  return catalog.currentUser(req.headers.cookie);
}

function requireUser(req: IncomingMessage): CatalogUser {
  const user = currentUser(req);
  if (!user) throw Object.assign(new Error("unauthorized"), { status: 401 });
  return user;
}

function authorizeWorkspace(req: IncomingMessage, workspaceId: string): CatalogWorkspace {
  return catalog.authorizeWorkspace(requireUser(req), workspaceId);
}

function parseMessageRequest(body: any): { content: string; attachments: MessageAttachment[] } {
  if (body?.content !== undefined && typeof body.content !== "string") {
    throw Object.assign(new Error("content is invalid"), { status: 400 });
  }
  return {
    content: typeof body?.content === "string" ? body.content : "",
    attachments: parseMessageAttachments(body?.attachments)
  };
}

function parseMessageAttachments(value: unknown): MessageAttachment[] {
  if (value === undefined) return [];
  if (!Array.isArray(value)) {
    throw Object.assign(new Error("attachments are invalid"), { status: 400 });
  }
  if (value.length > MAX_MESSAGE_ATTACHMENTS) {
    throw Object.assign(new Error(`maximum ${MAX_MESSAGE_ATTACHMENTS} attachments allowed`), { status: 400 });
  }

  let totalBytes = 0;
  return value.map((raw, index) => {
    if (!raw || typeof raw !== "object") {
      throw Object.assign(new Error("attachment is invalid"), { status: 400 });
    }
    const attachment = raw as Record<string, unknown>;
    const name = cleanAttachmentText(attachment.name, `attachment-${index + 1}`);
    const mimeType = cleanAttachmentText(attachment.mimeType, "application/octet-stream");
    const dataBase64 = typeof attachment.dataBase64 === "string" ? attachment.dataBase64.trim() : "";
    if (!dataBase64) {
      throw Object.assign(new Error(`${name} is missing file data`), { status: 400 });
    }
    if (!/^[A-Za-z0-9+/]+={0,2}$/.test(dataBase64)) {
      throw Object.assign(new Error(`${name} has invalid file data`), { status: 400 });
    }

    const bytes = Buffer.from(dataBase64, "base64").byteLength;
    if (bytes > MAX_MESSAGE_ATTACHMENT_BYTES) {
      throw Object.assign(new Error(`${name} exceeds the per-file upload limit`), { status: 400 });
    }
    totalBytes += bytes;
    if (totalBytes > MAX_MESSAGE_ATTACHMENT_TOTAL_BYTES) {
      throw Object.assign(new Error("attachments exceed the total upload limit"), { status: 400 });
    }

    return {
      id: cleanAttachmentText(attachment.id, `${Date.now()}-${index + 1}`),
      name,
      mimeType,
      size: bytes,
      dataBase64
    };
  });
}

function cleanAttachmentText(value: unknown, fallback: string): string {
  if (typeof value !== "string") return fallback;
  const trimmed = value.trim();
  return trimmed ? trimmed.slice(0, 240) : fallback;
}

async function shutdown(): Promise<void> {
  await telegram?.stop();
  runtimes.dispose();
  await vite?.close();
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 1000).unref();
}

function startTelegramChannel(): void {
  // The bot long-polls getUpdates; the token only works on one host at a
  // time. This flag lets a standby host run with full secrets without
  // fighting the active host for updates.
  if (process.env.AGENTMOM_TELEGRAM_DISABLED === "1") {
    console.log("telegram channel disabled via AGENTMOM_TELEGRAM_DISABLED");
    return;
  }
  const token = config.telegram.botToken?.trim();
  if (!token) return;

  telegram = new TelegramChannel({ token, catalog, runtimes });
  telegram.start();
}

async function startServer(): Promise<void> {
  const rawPort = process.env.AGENTMOM_PORT?.trim();
  const explicitPort = rawPort !== undefined && rawPort !== "";
  try {
    config.port = await listenOn(config.port);
  } catch (error) {
    if (isProduction || explicitPort || !isAddressInUse(error)) throw error;

    console.warn(`port ${config.port} is busy; choosing an ephemeral dev port`);
    config.port = await listenOn(0);
  }

  console.log(`agentmom listening on http://${config.host}:${config.port}`);
  console.log(`authEnabled=${config.authEnabled}`);
  console.log(`workspaceRoot=${config.workspaceRoot}`);
  console.log(`workspace=${config.workspace}`);
  console.log(`agentCwd=${config.agentCwd}`);
  console.log(`executor=${config.executor}`);
  console.log(`model=openrouter/${config.openRouterModel}`);
  console.log(`thinkingLevel=${config.thinkingLevel}`);
}

function listenOn(port: number): Promise<number> {
  return new Promise((resolvePromise, reject) => {
    const onError = (error: Error) => {
      cleanup();
      reject(error);
    };
    const onListening = () => {
      cleanup();
      const address = server.address();
      resolvePromise(address && typeof address === "object" ? address.port : port);
    };
    const cleanup = () => {
      server.off("error", onError);
      server.off("listening", onListening);
    };

    server.once("error", onError);
    server.once("listening", onListening);
    server.listen(port, config.host);
  });
}
