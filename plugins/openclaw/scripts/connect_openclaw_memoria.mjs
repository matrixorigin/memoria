#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function fail(message) {
  console.error(`[thememoria] ${message}`);
  process.exit(1);
}

function readArg(name, fallback = "") {
  const index = process.argv.indexOf(name);
  if (index >= 0 && index + 1 < process.argv.length) {
    return process.argv[index + 1];
  }
  return fallback;
}

function configObject(value, field) {
  if (value === undefined) return {};
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail(`invalid config: ${field} must be an object`);
  }
  return value;
}

function normalizeUrl(value) {
  return value.trim().replace(/\/+$/, "");
}

function resolveExecutable(command) {
  if (!command) {
    return null;
  }
  if (command.includes("/") || command.includes("\\")) {
    return fs.existsSync(command) ? path.resolve(command) : null;
  }
  const result = spawnSync("/usr/bin/env", ["which", command], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  if (result.status !== 0 || typeof result.stdout !== "string") {
    return null;
  }
  const resolved = result.stdout.trim().split(/\r?\n/).find(Boolean);
  return resolved ? path.resolve(resolved) : null;
}

function binaryLacksLocalEmbeddingSupport(executablePath) {
  try {
    const content = fs.readFileSync(executablePath);
    return content.includes(Buffer.from("compiled without local-embedding feature", "utf8"));
  } catch {
    return false;
  }
}

const modeRaw = readArg("--mode", "cloud").trim().toLowerCase();
if (modeRaw !== "cloud" && modeRaw !== "local") {
  fail("mode must be one of: cloud, local");
}
const mode = modeRaw;

const pluginId = readArg("--plugin-id", "thememoria").trim() || "thememoria";
const configFile = path.resolve(
  readArg(
    "--config-file",
    path.join(process.env.HOME || "", ".openclaw", "openclaw.json"),
  ),
);

const apiUrl = readArg("--api-url", "").trim();
const apiKey = readArg("--api-key", "").trim();
const dbUrl = readArg("--db-url", "").trim();
const memoriaExecutable = readArg("--memoria-executable", "").trim();
const defaultUserId = readArg("--default-user-id", "").trim();
const embeddingProviderArg = readArg("--embedding-provider", "").trim();
const embeddingModelArg = readArg("--embedding-model", "").trim();
const embeddingApiKeyArg = readArg("--embedding-api-key", "").trim();
const embeddingBaseUrlArg = readArg("--embedding-base-url", "").trim();
const embeddingDimArg = readArg("--embedding-dim", "").trim();

const embeddingDim =
  embeddingDimArg.length > 0 ? Number.parseInt(embeddingDimArg, 10) : undefined;
if (embeddingDimArg.length > 0 && (!Number.isFinite(embeddingDim) || embeddingDim < 1)) {
  fail("embedding-dim must be a positive integer");
}

let data = {};
if (fs.existsSync(configFile)) {
  try {
    data = JSON.parse(fs.readFileSync(configFile, "utf8"));
  } catch (error) {
    fail(`failed to parse config file ${configFile}: ${String(error)}`);
  }
} else {
  fs.mkdirSync(path.dirname(configFile), { recursive: true });
}

const root = configObject(data, "root");
const plugins = configObject(root.plugins, "plugins");
const entries = configObject(plugins.entries, "plugins.entries");
const slots = configObject(plugins.slots, "plugins.slots");
const allow = plugins.allow === undefined ? [] : plugins.allow;
if (!Array.isArray(allow) || !allow.every((entry) => typeof entry === "string" && entry.trim())) {
  fail("invalid config: plugins.allow must be an array of non-empty strings");
}
const entryPath = `plugins.entries.${pluginId}`;
const pluginEntry = configObject(Object.hasOwn(entries, pluginId) ? entries[pluginId] : undefined, entryPath);
const pluginConfig = configObject(pluginEntry.config, `${entryPath}.config`);

if (mode === "cloud") {
  if (!apiUrl) {
    fail("--api-url required when mode=cloud");
  }
  if (!apiKey) {
    fail("--api-key required when mode=cloud");
  }
  pluginConfig.backend = "api";
  pluginConfig.apiUrl = normalizeUrl(apiUrl);
  pluginConfig.apiKey = apiKey;
  delete pluginConfig.dbUrl;
  delete pluginConfig.embeddingProvider;
  delete pluginConfig.embeddingModel;
  delete pluginConfig.embeddingApiKey;
  delete pluginConfig.embeddingBaseUrl;
  delete pluginConfig.embeddingDim;
} else {
  if (!dbUrl) {
    fail("--db-url required when mode=local");
  }

  const embeddingProvider = embeddingProviderArg || String(pluginConfig.embeddingProvider || "openai");
  const embeddingModel =
    embeddingModelArg || String(pluginConfig.embeddingModel || "text-embedding-3-small");
  const embeddingApiKey =
    embeddingApiKeyArg || (typeof pluginConfig.embeddingApiKey === "string" ? pluginConfig.embeddingApiKey.trim() : "");

  if (embeddingProvider !== "local" && !embeddingApiKey) {
    fail("--embedding-api-key required for mode=local when embedding-provider is not 'local'");
  }

  const effectiveMemoriaExecutable =
    memoriaExecutable ||
    (typeof pluginConfig.memoriaExecutable === "string" ? pluginConfig.memoriaExecutable.trim() : "") ||
    "memoria";
  if (embeddingProvider === "local") {
    const resolvedExecutable = resolveExecutable(effectiveMemoriaExecutable);
    if (!resolvedExecutable) {
      fail(
        `embedding-provider=local requires a usable memoria executable, but '${effectiveMemoriaExecutable}' could not be resolved. Pass --memoria-executable <path> or install memoria first.`,
      );
    }
    if (binaryLacksLocalEmbeddingSupport(resolvedExecutable)) {
      fail(
        `embedding-provider=local requires a memoria binary built with local-embedding support. Resolved '${resolvedExecutable}', but this binary was built without that feature. Use a remote embedding provider or install/rebuild a local-embedding-enabled memoria binary.`,
      );
    }
  }

  pluginConfig.backend = "embedded";
  pluginConfig.dbUrl = dbUrl;
  pluginConfig.embeddingProvider = embeddingProvider;
  pluginConfig.embeddingModel = embeddingModel;
  delete pluginConfig.apiUrl;
  delete pluginConfig.apiKey;
  if (embeddingApiKey) {
    pluginConfig.embeddingApiKey = embeddingApiKey;
  }
  if (embeddingBaseUrlArg) {
    pluginConfig.embeddingBaseUrl = normalizeUrl(embeddingBaseUrlArg);
  }
  if (typeof embeddingDim === "number" && Number.isFinite(embeddingDim)) {
    pluginConfig.embeddingDim = embeddingDim;
  }
}

if (memoriaExecutable) {
  pluginConfig.memoriaExecutable = memoriaExecutable;
}
if (defaultUserId) {
  pluginConfig.defaultUserId = defaultUserId;
}

pluginEntry.enabled = true;
pluginEntry.config = pluginConfig;
slots.memory = pluginId;
if (!allow.includes(pluginId)) {
  allow.push(pluginId);
}
plugins.allow = allow;
plugins.entries = { ...entries, [pluginId]: pluginEntry };
plugins.slots = slots;
root.plugins = plugins;

fs.writeFileSync(configFile, `${JSON.stringify(root, null, 2)}\n`, "utf8");

console.log(
  JSON.stringify(
    {
      ok: true,
      mode,
      configFile,
      pluginId,
      backend: pluginConfig.backend,
      apiUrl:
        pluginConfig.backend === "api" && typeof pluginConfig.apiUrl === "string"
          ? pluginConfig.apiUrl
          : undefined,
      apiKeySet:
        pluginConfig.backend === "api" &&
        typeof pluginConfig.apiKey === "string" &&
        pluginConfig.apiKey.length > 0,
      dbUrl: typeof pluginConfig.dbUrl === "string" ? pluginConfig.dbUrl : undefined,
      memoriaExecutable:
        typeof pluginConfig.memoriaExecutable === "string"
          ? pluginConfig.memoriaExecutable
          : undefined,
    },
    null,
    2,
  ),
);
