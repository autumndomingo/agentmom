import { existsSync, readFileSync, statSync } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { extname, join } from "node:path";
import { SESSION_COOKIE } from "./catalog.js";
import { requestHeaders } from "./proxy-utils.js";

export function isSecureRequest(req: IncomingMessage): boolean {
  return req.headers["x-forwarded-proto"] === "https";
}

export function deploymentRequestHeaders(req: IncomingMessage): Record<string, string> {
  const headers = requestHeaders(req.headers);
  delete headers.cookie;
  delete headers.authorization;
  delete headers["proxy-authorization"];

  const host = firstHeader(req.headers.host);
  if (host) {
    headers.host = host;
    headers["x-forwarded-host"] = host;
  }

  headers["x-forwarded-proto"] = firstHeader(req.headers["x-forwarded-proto"]) || (isSecureRequest(req) ? "https" : "http");
  return headers;
}

export function previewRequestHeaders(req: IncomingMessage): Record<string, string> {
  const headers = requestHeaders(req.headers);
  const cookie = stripCookie(headers.cookie, SESSION_COOKIE);
  if (cookie) {
    headers.cookie = cookie;
  } else {
    delete headers.cookie;
  }
  return headers;
}

export function stripCookie(cookieHeader: string | undefined, name: string): string | undefined {
  if (!cookieHeader) return undefined;
  const next = cookieHeader
    .split(";")
    .map((part) => part.trim())
    .filter((part) => {
      const equals = part.indexOf("=");
      const key = equals === -1 ? part : part.slice(0, equals);
      return key !== name;
    })
    .join("; ");
  return next || undefined;
}

export function firstHeader(value: string | string[] | undefined): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

export async function readJson(req: IncomingMessage): Promise<any> {
  const body = await readBody(req);
  if (body.length === 0) return undefined;
  return JSON.parse(body.toString("utf8"));
}

export async function readBody(req: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}

export function sendJson(
  res: ServerResponse,
  payload: unknown,
  status = 200,
  headers: Record<string, string> = {}
): void {
  res.writeHead(status, { "Content-Type": "application/json", ...headers });
  res.end(JSON.stringify(payload));
}

export function sendProxyResponse(
  res: ServerResponse,
  status: number,
  headers: Record<string, string>,
  body: Buffer | undefined
): void {
  res.writeHead(status, headers);
  res.end(body);
}

export function sendError(res: ServerResponse, error: unknown, status = 500): void {
  if (res.headersSent) {
    res.end();
    return;
  }
  const message = error instanceof Error ? error.message : String(error);
  sendJson(res, { error: message }, status);
}

export function errorStatus(error: unknown): number {
  const explicit = typeof error === "object" && error && "status" in error ? Number((error as { status?: unknown }).status) : 0;
  if (explicit) return explicit;
  const message = error instanceof Error ? error.message : String(error);
  if (message === "forbidden" || message === "admin required") return 403;
  if (message.includes("not found")) return 404;
  if (message === "unauthorized") return 401;
  if (message.includes("required") || message.includes("invalid") || message.includes("registered")) {
    return 400;
  }
  return 500;
}

export function serveStatic(rootDir: string, pathname: string, res: ServerResponse): void {
  const clientDir = join(rootDir, "dist/client");
  const relativePath = pathname === "/" ? "/index.html" : pathname;
  const requestedFile = join(clientDir, relativePath);
  const directoryIndex = join(requestedFile, "index.html");
  const filePath = isFile(requestedFile)
    ? requestedFile
    : isFile(directoryIndex)
      ? directoryIndex
    : extname(pathname) === ""
      ? join(clientDir, "index.html")
      : undefined;
  if (!filePath || !isFile(filePath)) return sendError(res, new Error("Not found"), 404);

  const mime =
    extname(filePath) === ".html"
      ? "text/html"
      : extname(filePath) === ".js"
        ? "text/javascript"
        : extname(filePath) === ".css"
          ? "text/css"
          : "application/octet-stream";
  res.writeHead(200, { "Content-Type": mime });
  res.end(readFileSync(filePath));
}

function isFile(path: string): boolean {
  try {
    return existsSync(path) && statSync(path).isFile();
  } catch {
    return false;
  }
}

export function isAddressInUse(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "EADDRINUSE";
}
