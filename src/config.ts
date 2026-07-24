import { existsSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export type AppConfig = {
  appCommit?: string;
  authEnabled: boolean;
  host: string;
  port: number;
  executor: "local" | "fly";
  stateDir: string;
  workspace: string;
  workspaceId?: string;
  workspaceDirName?: string;
  workspaceRoot: string;
  projectsDir: string;
  agentCwd: string;
  agentDir: string;
  sessionDir: string;
  deploymentDir: string;
  deploymentBaseDomain?: string;
  previewBasePath: string;
  openRouterModel: string;
  thinkingLevel: ThinkingLevel;
  appEnvFile: string;
  openRouterApiKey?: string;
  braveApiKey?: string;
  telegram: {
    botToken?: string;
  };
  rootDir: string;
  podman: {
    command: string;
  };
  fly: {
    token: string;
    org: string;
    region: string;
    image: string;
    cpus: number;
    memoryMb: number;
    volumeGb: number;
    idleMinutes: number;
    appPrefix: string;
    deployAppPrefix: string;
    serverApp?: string;
    flyctl: string;
    shimUrl: string;
  };
  deploy: {
    memoryMb: number;
    cpus: number;
    pidsLimit: number;
    maxPerWorkspace: number;
    idleMinutes: number;
  };
};

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const THINKING_LEVELS = ["minimal", "low", "medium", "high", "xhigh"] as const;

export type ThinkingLevel = (typeof THINKING_LEVELS)[number];

export type LoadConfigOptions = {
  requireServiceSecrets?: boolean;
};

function readEnvFileValue(path: string, name: string): string | undefined {
  if (!existsSync(path)) return undefined;

  const content = readFileSync(path, "utf8");
  for (const rawLine of content.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;

    const match = /^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line);
    if (!match || match[1] !== name) continue;

    let value = match[2].trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    return value;
  }

  return undefined;
}

function envFilePath(): string {
  return resolve(process.env.AGENTMOM_ENV_FILE ?? `${rootDir}/.env`);
}

function readOpenRouterKeyFile(path: string): string | undefined {
  const value = readEnvFileValue(path, "OPENROUTER_API_KEY");
  if (value || !existsSync(path)) return value;
  const rawKey = readFileSync(path, "utf8").trim();
  return rawKey && !rawKey.includes("\n") ? rawKey : undefined;
}

function numberFromEnv(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) return fallback;
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) ? value : fallback;
}

function boolFromEnv(name: string, fallback: boolean): boolean {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  return raw === "1" || raw.toLowerCase() === "true" || raw.toLowerCase() === "yes";
}

function domainFromEnv(name: string): string | undefined {
  const raw = process.env[name]?.trim().toLowerCase();
  if (!raw) return undefined;
  return raw.replace(/^https?:\/\//, "").replace(/\/.*$/, "").replace(/\.$/, "");
}

function thinkingLevelFromEnv(name: string, fallback: ThinkingLevel): ThinkingLevel {
  const raw = process.env[name]?.trim().toLowerCase();
  if (!raw) return fallback;
  if ((THINKING_LEVELS as readonly string[]).includes(raw)) return raw as ThinkingLevel;
  throw new Error(`Invalid ${name}: ${raw}; expected one of ${THINKING_LEVELS.join(", ")}`);
}

function readGitCommit(): string | undefined {
  try {
    return execFileSync("git", ["-c", `safe.directory=${rootDir}`, "rev-parse", "--short=12", "HEAD"], {
      cwd: rootDir,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"]
    }).trim();
  } catch {
    return undefined;
  }
}

function requireServiceSecrets(config: {
  appEnvFile: string;
  openRouterApiKey?: string;
  braveApiKey?: string;
  telegramBotToken?: string;
}): void {
  const missing: string[] = [];
  if (!config.openRouterApiKey) missing.push("OPENROUTER_API_KEY");
  if (!config.telegramBotToken) missing.push("AGENTMOM_TELEGRAM_BOT_TOKEN");
  if (!config.braveApiKey) missing.push("BRAVE_API_KEY");

  if (missing.length === 0) return;

  const sourceHint = existsSync(config.appEnvFile)
    ? `env file: ${config.appEnvFile}`
    : `missing env file: ${config.appEnvFile}`;
  throw new Error(`Missing required app secrets (${missing.join(", ")}); checked ${sourceHint}`);
}

function executorFromEnv(): "local" | "fly" {
  return process.env.AGENTMOM_EXECUTOR === "fly" ? "fly" : "local";
}

function flyToken(): string {
  const direct = process.env.AGENTMOM_FLY_API_TOKEN?.trim();
  if (direct) return direct;
  const tokenFile = process.env.AGENTMOM_FLY_API_TOKEN_FILE?.trim();
  if (tokenFile && existsSync(tokenFile)) {
    return readFileSync(tokenFile, "utf8").trim();
  }
  return "";
}

export function loadConfig(options: LoadConfigOptions = {}): AppConfig {
  const workspace = resolve(process.env.AGENTMOM_WORKSPACE ?? process.cwd());
  const workspaceRoot = resolve(process.env.AGENTMOM_WORKSPACE_ROOT ?? `${workspace}/workspaces`);
  const projectsDir = resolve(process.env.AGENTMOM_PROJECTS_DIR ?? `${workspace}/projects`);
  const agentCwd = resolve(process.env.AGENTMOM_AGENT_CWD ?? projectsDir);
  const appEnvFile = envFilePath();
  const openRouterApiKey = process.env.OPENROUTER_API_KEY ?? readOpenRouterKeyFile(appEnvFile);
  const braveApiKey =
    process.env.BRAVE_API_KEY ??
    process.env.BRAVE_SEARCH_API_KEY ??
    readEnvFileValue(appEnvFile, "BRAVE_API_KEY") ??
    readEnvFileValue(appEnvFile, "BRAVE_SEARCH_API_KEY");
  const telegramBotToken =
    process.env.AGENTMOM_TELEGRAM_BOT_TOKEN ??
    readEnvFileValue(appEnvFile, "AGENTMOM_TELEGRAM_BOT_TOKEN");

  if (openRouterApiKey && !process.env.OPENROUTER_API_KEY) {
    process.env.OPENROUTER_API_KEY = openRouterApiKey;
  }
  if (braveApiKey && !process.env.BRAVE_API_KEY) {
    process.env.BRAVE_API_KEY = braveApiKey;
  }
  if (telegramBotToken && !process.env.AGENTMOM_TELEGRAM_BOT_TOKEN) {
    process.env.AGENTMOM_TELEGRAM_BOT_TOKEN = telegramBotToken;
  }

  const stateDir = resolve(process.env.AGENTMOM_STATE_DIR ?? `${workspace}/.agentmom`);

  if (options.requireServiceSecrets) {
    requireServiceSecrets({ appEnvFile, openRouterApiKey, braveApiKey, telegramBotToken });
  }

  return {
    appCommit: process.env.AGENTMOM_COMMIT ?? readGitCommit(),
    authEnabled: boolFromEnv("AGENTMOM_AUTH_ENABLED", process.env.NODE_ENV === "production"),
    host: process.env.AGENTMOM_HOST ?? "127.0.0.1",
    port: numberFromEnv("AGENTMOM_PORT", 7392),
    executor: executorFromEnv(),
    stateDir,
    workspace,
    workspaceRoot,
    projectsDir,
    agentCwd,
    agentDir: resolve(process.env.AGENTMOM_AGENT_DIR ?? `${stateDir}/pi`),
    sessionDir: resolve(process.env.AGENTMOM_SESSION_DIR ?? `${stateDir}/sessions`),
    deploymentDir: resolve(process.env.AGENTMOM_DEPLOYMENT_DIR ?? `${stateDir}/deployments`),
    deploymentBaseDomain: domainFromEnv("AGENTMOM_DEPLOYMENT_BASE_DOMAIN"),
    previewBasePath: process.env.AGENTMOM_PREVIEW_BASE_PATH ?? "/preview",
    openRouterModel: process.env.AGENTMOM_OPENROUTER_MODEL ?? "openai/gpt-5.6-luna",
    thinkingLevel: thinkingLevelFromEnv("AGENTMOM_THINKING_LEVEL", "low"),
    appEnvFile,
    openRouterApiKey,
    braveApiKey,
    telegram: {
      botToken: telegramBotToken
    },
    rootDir,
    deploy: {
      memoryMb: numberFromEnv("AGENTMOM_DEPLOY_MEMORY_MB", 512),
      cpus: numberFromEnv("AGENTMOM_DEPLOY_CPUS", 1),
      pidsLimit: numberFromEnv("AGENTMOM_DEPLOY_PIDS_LIMIT", 256),
      maxPerWorkspace: numberFromEnv("AGENTMOM_DEPLOY_MAX_PER_WORKSPACE", 5),
      idleMinutes: numberFromEnv("AGENTMOM_DEPLOY_IDLE_MINUTES", 15)
    },
    podman: {
      command: process.env.AGENTMOM_PODMAN_COMMAND ?? "podman"
    },
    fly: {
      token: flyToken(),
      org: process.env.AGENTMOM_FLY_ORG ?? "personal",
      region: process.env.AGENTMOM_FLY_REGION ?? "arn",
      image: process.env.AGENTMOM_FLY_IMAGE ?? "docker.io/library/node:24-bookworm",
      cpus: numberFromEnv("AGENTMOM_FLY_CPUS", 2),
      memoryMb: numberFromEnv("AGENTMOM_FLY_MEMORY_MB", 2048),
      volumeGb: numberFromEnv("AGENTMOM_FLY_VOLUME_GB", 10),
      idleMinutes: numberFromEnv("AGENTMOM_FLY_IDLE_MINUTES", 10),
      appPrefix: process.env.AGENTMOM_FLY_APP_PREFIX ?? "am-ws-",
      deployAppPrefix: process.env.AGENTMOM_FLY_DEPLOY_APP_PREFIX ?? "am-dep-",
      serverApp: process.env.AGENTMOM_FLY_SERVER_APP,
      flyctl: process.env.AGENTMOM_FLYCTL_COMMAND ?? "flyctl",
      shimUrl:
        process.env.AGENTMOM_FLY_SHIM_URL ??
        (domainFromEnv("AGENTMOM_DEPLOYMENT_BASE_DOMAIN")
          ? `https://${domainFromEnv("AGENTMOM_DEPLOYMENT_BASE_DOMAIN")}/api/sandbox-shim`
          : "")
    },
  };
}
