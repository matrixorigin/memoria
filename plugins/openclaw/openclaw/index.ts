import path from "node:path";
import { fileURLToPath } from "node:url";
import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import {
  MEMORIA_MEMORY_TYPES,
  MEMORIA_TRUST_TIERS,
  memoriaPluginConfigSchema,
  parseMemoriaPluginConfig,
  type MemoriaMemoryType,
  type MemoriaPluginConfig,
  type MemoriaTrustTier,
} from "./config.js";
import {
  MemoriaClient,
  type MemoriaMemoryRecord,
  type MemoriaStatsResponse,
} from "./client.js";
import { formatMemoryList, formatRelevantMemoriesContext } from "./format.js";
import { resolveMemorySearchScore } from "./score.js";

type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  details: Record<string, unknown>;
};

type PluginIdentityContext = {
  agentId?: string;
  sessionKey?: string;
  sessionId?: string;
};

const EMPTY_OBJECT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  properties: {},
} as const;

const MEMORIA_AGENT_GUIDANCE = [
  "Memoria is the durable external memory system for this runtime.",
  "When the user asks you to remember, update, forget, correct, snapshot, or restore memory, prefer the Memoria tools over editing local workspace memory files.",
  "Use memory_store for new durable facts or preferences, memory_correct or memory_forget for repairs, and memory_snapshot plus memory_rollback when you need a recoverable checkpoint.",
  "Workspace files like MEMORY.md and memory/YYYY-MM-DD.md are separate local notes. Only edit them when the user explicitly asks for file-based memory or workspace-local notes.",
  "Do not claim that only memory_search or memory_get are available when other memory_* tools are present in the tool list.",
].join("\n");

const PLUGIN_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function resolveOpenClawBinFromProcess(): string {
  return typeof process.argv[1] === "string" && process.argv[1].trim()
    ? process.argv[1]
    : "openclaw";
}

function resolveOpenClawConfigFile(): string {
  const home = process.env.HOME ?? "";
  const explicitConfigPath = process.env.OPENCLAW_CONFIG_PATH?.trim() ?? "";
  if (explicitConfigPath) {
    return explicitConfigPath.replace(/^~(?=$|\/|\\)/, home);
  }
  const openclawHome = process.env.OPENCLAW_HOME?.trim() ?? "";
  return openclawHome
    ? path.join(openclawHome, ".openclaw", "openclaw.json")
    : path.join(home, ".openclaw", "openclaw.json");
}

function objectSchema(
  properties: Record<string, unknown>,
  required: string[] = [],
): Record<string, unknown> {
  return {
    type: "object",
    additionalProperties: false,
    properties,
    ...(required.length > 0 ? { required } : {}),
  };
}

function jsonResult(payload: Record<string, unknown>): ToolResult {
  return {
    content: [{ type: "text", text: JSON.stringify(payload, null, 2) }],
    details: payload,
  };
}

function textResult(text: string, details: Record<string, unknown> = {}): ToolResult {
  return {
    content: [{ type: "text", text }],
    details,
  };
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function extractTextContent(content: unknown): string {
  if (typeof content === "string") {
    return content.trim();
  }
  if (!Array.isArray(content)) {
    return "";
  }
  const parts: string[] = [];
  for (const item of content) {
    const block = asRecord(item);
    if (!block || block.type !== "text" || typeof block.text !== "string") {
      continue;
    }
    const text = block.text.trim();
    if (text) {
      parts.push(text);
    }
  }
  return parts.join("\n").trim();
}

function collectRecentConversationMessages(
  messages: unknown[],
  options: { tailMessages: number; maxChars: number },
): Array<{ role: string; content: string }> {
  const normalized: Array<{ role: string; content: string }> = [];

  for (const entry of messages) {
    const message = asRecord(entry);
    if (!message) {
      continue;
    }
    const role = typeof message.role === "string" ? message.role.trim() : "";
    if (role !== "user" && role !== "assistant") {
      continue;
    }
    const text = extractTextContent(message.content);
    if (!text) {
      continue;
    }
    normalized.push({ role, content: text });
  }

  const tail = normalized.slice(-options.tailMessages);
  const output: Array<{ role: string; content: string }> = [];
  let usedChars = 0;

  for (let index = tail.length - 1; index >= 0; index -= 1) {
    const current = tail[index];
    if (usedChars >= options.maxChars) {
      break;
    }
    const remaining = options.maxChars - usedChars;
    const content =
      current.content.length > remaining ? current.content.slice(-remaining) : current.content;
    usedChars += content.length;
    output.unshift({ role: current.role, content });
  }

  return output;
}

function readString(
  params: Record<string, unknown>,
  key: string,
  options: { required?: boolean; label?: string } = {},
): string | undefined {
  const { required = false, label = key } = options;
  const raw = params[key];
  if (typeof raw !== "string" || !raw.trim()) {
    if (required) {
      throw new Error(`${label} required`);
    }
    return undefined;
  }
  return raw.trim();
}

function readNumber(params: Record<string, unknown>, key: string): number | undefined {
  const raw = params[key];
  return typeof raw === "number" && Number.isFinite(raw) ? raw : undefined;
}

function readBoolean(params: Record<string, unknown>, key: string): boolean | undefined {
  const raw = params[key];
  return typeof raw === "boolean" ? raw : undefined;
}

function clampInt(value: number | undefined, min: number, max: number, fallback: number): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

function readMemoryType(
  params: Record<string, unknown>,
  key: string,
): MemoriaMemoryType | undefined {
  const raw = readString(params, key);
  if (!raw) {
    return undefined;
  }
  if (!MEMORIA_MEMORY_TYPES.includes(raw as MemoriaMemoryType)) {
    throw new Error(`${key} must be one of ${MEMORIA_MEMORY_TYPES.join(", ")}`);
  }
  return raw as MemoriaMemoryType;
}

function readTrustTier(
  params: Record<string, unknown>,
  key: string,
): MemoriaTrustTier | undefined {
  const raw = readString(params, key);
  if (!raw) {
    return undefined;
  }
  if (!MEMORIA_TRUST_TIERS.includes(raw as MemoriaTrustTier)) {
    throw new Error(`${key} must be one of ${MEMORIA_TRUST_TIERS.join(", ")}`);
  }
  return raw as MemoriaTrustTier;
}

function readToolTopK(params: Record<string, unknown>, fallback: number): number {
  return clampInt(readNumber(params, "topK") ?? readNumber(params, "maxResults"), 1, 20, fallback);
}

function readObjectArray(raw: unknown, label: string): Array<Record<string, unknown>> {
  if (!Array.isArray(raw)) {
    throw new Error(`${label} must be an array`);
  }
  return raw.map((entry) => {
    const record = asRecord(entry);
    if (!record) {
      throw new Error(`${label} must be an array of objects`);
    }
    return record;
  });
}

function readEntityPayload(
  params: Record<string, unknown>,
  key: string,
): Array<Record<string, unknown>> {
  const raw = params[key];
  if (Array.isArray(raw)) {
    return readObjectArray(raw, key);
  }
  if (typeof raw === "string" && raw.trim()) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      throw new Error(`${key} must be valid JSON or an array`);
    }
    return readObjectArray(parsed, key);
  }
  throw new Error(`${key} required`);
}

function readObserveMessages(
  params: Record<string, unknown>,
  key: string,
): Array<{ role: string; content: string }> {
  const records = readObjectArray(params[key], key);
  return records.map((message) => {
    const role = readString(message, "role", { required: true, label: "messages[].role" })!;
    const content = readString(message, "content", {
      required: true,
      label: "messages[].content",
    })!;
    return { role, content };
  });
}

function readStringList(
  params: Record<string, unknown>,
  key: string,
): string[] | undefined {
  const raw = params[key];
  if (raw === undefined) {
    return undefined;
  }
  if (!Array.isArray(raw)) {
    throw new Error(`${key} must be an array of strings`);
  }
  const items = raw.map((entry) => {
    if (typeof entry !== "string" || !entry.trim()) {
      throw new Error(`${key} must be an array of strings`);
    }
    return entry.trim();
  });
  return items.length > 0 ? items : undefined;
}

function buildMemoryPath(memoryId: string): string {
  return `memoria://${memoryId}`;
}

const CLI_COMMAND_NAMES = ["memoria", "ltm"] as const;

const MEMORY_TOOL_ALIASES: Record<string, string> = {
  memory_recall: "memory_retrieve",
  "ltm list": "memory_list",
  "ltm search": "memory_recall",
  "ltm stats": "memory_stats",
  "ltm health": "memory_health",
};

function buildMemoryStatsPayload(
  config: MemoriaPluginConfig,
  userId: string,
  stats: MemoriaStatsResponse,
): Record<string, unknown> {
  return {
    backend: config.backend,
    userId,
    activeMemoryCount: stats.activeMemoryCount,
    inactiveMemoryCount: stats.inactiveMemoryCount,
    byType: stats.byType,
    entityCount: stats.entityCount,
    snapshotCount: stats.snapshotCount,
    branchCount: stats.branchCount,
    healthWarnings: stats.healthWarnings,
    autoRecall: config.autoRecall,
    autoObserve: config.autoObserve,
    supportsRollback: true,
    supportsBranches: true,
    partial: stats.partial ?? false,
    limitations: stats.limitations ?? [],
  };
}

function buildCapabilitiesPayload(config: MemoriaPluginConfig): Record<string, unknown> {
  const limitations = [
    "OpenClaw reserves `openclaw memory` for built-in file-memory commands; compatibility CLI is exposed as `openclaw ltm`.",
    "memory_get is served from recent tool results, or looked up by ID through the Memoria API on a cache miss; only active memories are returned.",
  ];

  return {
    backend: config.backend,
    userIdStrategy: config.userIdStrategy,
    autoRecall: config.autoRecall,
    autoObserve: config.autoObserve,
    llmConfigured: Boolean(config.llmApiKey || config.backend === "api"),
    tools: supportedToolNames(),
    embeddedOnly: [],
    cliCommands: [...CLI_COMMAND_NAMES],
    aliases: MEMORY_TOOL_ALIASES,
    backendFeatures: {
      rollback: true,
      snapshots: true,
      branches: true,
      governance: true,
      reflect: true,
      entities: true,
      rebuildIndex: true,
    },
    limitations,
  };
}

function sliceContent(content: string, from?: number, lines?: number): string {
  const allLines = content.split(/\r?\n/);
  const start = Math.max(0, (from ?? 1) - 1);
  const end = typeof lines === "number" && lines > 0 ? start + lines : allLines.length;
  return allLines.slice(start, end).join("\n");
}

function resolveUserId(
  config: MemoriaPluginConfig,
  ctx: PluginIdentityContext,
  explicitUserId?: string,
): string {
  if (explicitUserId?.trim()) {
    return explicitUserId.trim();
  }
  if (config.userIdStrategy === "sessionKey") {
    return ctx.sessionKey?.trim() || ctx.sessionId?.trim() || config.defaultUserId;
  }
  if (config.userIdStrategy === "agentId") {
    return ctx.agentId?.trim() || ctx.sessionKey?.trim() || config.defaultUserId;
  }
  return config.defaultUserId;
}

function toMemorySearchPayload(memories: MemoriaMemoryRecord[]) {
  return memories.map((memory) => ({
    path: buildMemoryPath(memory.memory_id),
    startLine: 1,
    endLine: Math.max(1, memory.content.split(/\r?\n/).length),
    score: resolveMemorySearchScore(memory),
    snippet: memory.content,
    source: "memory",
  }));
}

function supportedToolNames(): string[] {
  return [
    "memory_search",
    "memory_get",
    "memory_store",
    "memory_retrieve",
    "memory_recall",
    "memory_list",
    "memory_stats",
    "memory_profile",
    "memory_correct",
    "memory_purge",
    "memory_forget",
    "memory_health",
    "memory_observe",
    "memory_governance",
    "memory_consolidate",
    "memory_reflect",
    "memory_extract_entities",
    "memory_link_entities",
    "memory_rebuild_index",
    "memory_capabilities",
    "memory_snapshot",
    "memory_snapshots",
    "memory_rollback",
    "memory_branch",
    "memory_branches",
    "memory_checkout",
    "memory_branch_delete",
    "memory_merge",
    "memory_diff",
  ];
}

function hasMessageError(value: unknown): value is Record<string, unknown> & { message: string } {
  const record = asRecord(value);
  return Boolean(record && "error" in record && typeof record.message === "string");
}

function hasNonEmptyString(value: unknown): boolean {
  return typeof value === "string" && value.trim().length > 0;
}

function shouldShowOnboardingHint(rawPluginConfig: unknown): boolean {
  const raw = asRecord(rawPluginConfig);
  if (!raw) {
    return true;
  }

  const backend = hasNonEmptyString(raw.backend) ? String(raw.backend).trim().toLowerCase() : "";
  const hasCloudConfig = hasNonEmptyString(raw.apiUrl) || hasNonEmptyString(raw.apiKey);
  const hasLocalConfig = hasNonEmptyString(raw.dbUrl);

  return !(backend === "api" || hasCloudConfig || hasLocalConfig);
}

const ONBOARDING_HINT_ONCE_KEY = "__memory_memoria_onboarding_hint_logged__";

function shouldLogOnboardingHintOnce(): boolean {
  const state = globalThis as Record<string, unknown>;
  if (state[ONBOARDING_HINT_ONCE_KEY] === true) {
    return false;
  }
  state[ONBOARDING_HINT_ONCE_KEY] = true;
  return true;
}

const plugin = {
  id: "thememoria",
  name: "Memory (Memoria)",
  description: "Memoria-backed long-term memory plugin for OpenClaw. Uses direct HTTP API mode (no binary required).",
  kind: "memory" as const,
  configSchema: memoriaPluginConfigSchema,

  register(api: OpenClawPluginApi) {
    const config = parseMemoriaPluginConfig(api.pluginConfig);
    const client = new MemoriaClient(config);

    const needsSetup = shouldShowOnboardingHint(api.pluginConfig);
    const isFirstRegister = shouldLogOnboardingHintOnce();

    if (isFirstRegister) {
      api.logger.info(
        `thememoria: registered (${needsSetup ? "pending setup" : config.backend})`,
      );

      const isEnableCommand =
        process.argv.some((arg) => arg === "enable") &&
        process.argv.some((arg) => arg === "plugins");
      if (needsSetup && isEnableCommand) {
        api.logger.info(
          "🧠 Memoria next step (Cloud, recommended): openclaw memoria setup --mode cloud --api-url <MEMORIA_API_URL> --api-key <MEMORIA_API_KEY>",
        );
        api.logger.info(
          "🧩 Local quick start: openclaw memoria setup --mode local --install-memoria --embedding-api-key <EMBEDDING_API_KEY>",
        );
        api.logger.info(
          "📘 More options: openclaw memoria setup --help",
        );
        api.logger.info(
          "🧪 Verify with: openclaw memoria health",
        );
      }
    }

    api.on("before_prompt_build", async () => ({
      appendSystemContext: MEMORIA_AGENT_GUIDANCE,
    }));

    api.registerTool(
      (ctx) => {
        const userIdProperty = {
          type: "string",
          description: "Optional explicit Memoria user_id override",
        } as const;
        const forceProperty = {
          type: "boolean",
          description: "Skip cooldown when the backend supports it",
        } as const;
        const modeProperty = {
          type: "string",
          description: "auto uses internal LLM when configured, otherwise falls back to candidates",
          enum: ["auto", "internal", "candidates"],
        } as const;

        const memorySearchTool = {
          label: "Memory Search",
          name: "memory_search",
          description:
            "Search Memoria for prior work, preferences, facts, decisions, or todos before answering questions that depend on earlier context.",
          parameters: objectSchema(
            {
              query: { type: "string", description: "Natural-language memory query" },
              topK: {
                type: "integer",
                description: "Maximum number of results to return",
                minimum: 1,
                maximum: 20,
              },
              maxResults: {
                type: "integer",
                description: "Alias for topK",
                minimum: 1,
                maximum: 20,
              },
              userId: userIdProperty,
            },
            ["query"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const query = readString(params, "query", { required: true, label: "query" })!;
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const topK = readToolTopK(params, config.retrieveTopK);

            try {
              const memories = await client.search({
                userId,
                query,
                topK,
              });
              return jsonResult({
                provider: "memoria",
                backend: config.backend,
                userId,
                results: toMemorySearchPayload(memories),
                memories,
              });
            } catch (error) {
              return jsonResult({
                results: [],
                memories: [],
                disabled: true,
                unavailable: true,
                error: error instanceof Error ? error.message : String(error),
              });
            }
          },
        };

        const memoryGetTool = {
          label: "Memory Get",
          name: "memory_get",
          description: "Read a specific Memoria memory returned by memory_search.",
          parameters: objectSchema(
            {
              path: { type: "string", description: "memoria://<memory_id>" },
              from: { type: "integer", description: "Start line (1-based)", minimum: 1 },
              lines: { type: "integer", description: "Number of lines", minimum: 1 },
              userId: userIdProperty,
            },
            ["path"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const rawPath = readString(params, "path", { required: true, label: "path" })!;
            const memoryId = rawPath.startsWith("memoria://")
              ? rawPath.slice("memoria://".length)
              : "";
            if (!memoryId) {
              return jsonResult({
                path: rawPath,
                text: "",
                disabled: true,
                error: "invalid memoria path",
              });
            }

            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const from = clampInt(readNumber(params, "from"), 1, Number.MAX_SAFE_INTEGER, 1);
            const lines =
              readNumber(params, "lines") === undefined
                ? undefined
                : clampInt(readNumber(params, "lines"), 1, Number.MAX_SAFE_INTEGER, 1);

            try {
              const memory = await client.getMemory({ userId, memoryId });
              if (!memory) {
                return jsonResult({
                  path: rawPath,
                  text: "",
                  disabled: true,
                  error: "memory not found",
                });
              }
              return jsonResult({
                path: rawPath,
                text: sliceContent(memory.content, from, lines),
                memory,
              });
            } catch (error) {
              return jsonResult({
                path: rawPath,
                text: "",
                disabled: true,
                error: error instanceof Error ? error.message : String(error),
              });
            }
          },
        };

        const memoryHealthTool = {
          label: "Memory Health",
          name: "memory_health",
          description: "Check Memoria connectivity and health warnings for the current user.",
          parameters: objectSchema({
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const health = await client.health(userId);
            return jsonResult({
              userId,
              backend: config.backend,
              ...(asRecord(health) ?? {}),
            });
          },
        };

        const memoryStoreTool = {
          label: "Memory Store",
          name: "memory_store",
          description: "Store a durable memory in Memoria.",
          parameters: objectSchema(
            {
              content: { type: "string", description: "Memory content to store" },
              memoryType: {
                type: "string",
                description: `One of: ${MEMORIA_MEMORY_TYPES.join(", ")}`,
                enum: [...MEMORIA_MEMORY_TYPES],
              },
              trustTier: {
                type: "string",
                description: `Optional trust tier: ${MEMORIA_TRUST_TIERS.join(", ")}`,
                enum: [...MEMORIA_TRUST_TIERS],
              },
              sessionId: {
                type: "string",
                description: "Optional session scope for the memory",
              },
              source: {
                type: "string",
                description: "Optional source label",
              },
              userId: userIdProperty,
            },
            ["content"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const content = readString(params, "content", {
              required: true,
              label: "content",
            })!;
            const memoryType = readMemoryType(params, "memoryType") ?? "semantic";
            const trustTier = readTrustTier(params, "trustTier");
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const stored = await client.storeMemory({
              userId,
              content,
              memoryType,
              trustTier,
              sessionId: readString(params, "sessionId") ?? ctx.sessionId,
              source: readString(params, "source") ?? "openclaw:memory_store",
            });
            return textResult(`Stored memory ${stored.memory_id}.`, {
              ok: true,
              userId,
              path: buildMemoryPath(stored.memory_id),
              memory: stored,
            });
          },
        };

        const executeMemoryRetrieve = async (_toolCallId: string, rawParams: unknown) => {
          const params = asRecord(rawParams) ?? {};
          const query = readString(params, "query", { required: true, label: "query" })!;
          const userId = resolveUserId(config, ctx, readString(params, "userId"));
          const topK = readToolTopK(params, config.retrieveTopK);
          const sessionId = readString(params, "sessionId") ?? ctx.sessionId;

          const [memories, health] = await Promise.all([
            client.retrieve({
              userId,
              query,
              topK,
              memoryTypes: config.retrieveMemoryTypes,
              sessionId,
              includeCrossSession: config.includeCrossSession,
            }),
            client.health(userId).catch(() => null),
          ]);

          const warnings = Array.isArray(asRecord(health)?.warnings)
            ? (asRecord(health)?.warnings as unknown[]).filter(
                (entry): entry is string => typeof entry === "string" && entry.trim().length > 0,
              )
            : [];

          return jsonResult({
            backend: config.backend,
            userId,
            count: memories.length,
            warnings,
            memories,
          });
        };

        const memoryRetrieveParameters = objectSchema(
          {
            query: { type: "string", description: "Retrieval query" },
            topK: {
              type: "integer",
              description: "Maximum number of memories to retrieve",
              minimum: 1,
              maximum: 20,
            },
            maxResults: {
              type: "integer",
              description: "Alias for topK",
              minimum: 1,
              maximum: 20,
            },
            sessionId: {
              type: "string",
              description: "Optional session scope hint",
            },
            userId: userIdProperty,
          },
          ["query"],
        );

        const memoryRetrieveTool = {
          label: "Memory Retrieve",
          name: "memory_retrieve",
          description: "Retrieve the most relevant memories for a natural-language query.",
          parameters: memoryRetrieveParameters,
          execute: executeMemoryRetrieve,
        };

        const memoryRecallTool = {
          label: "Memory Recall",
          name: "memory_recall",
          description:
            "Compatibility alias for memory_retrieve, matching memory-lancedb-pro's recall tool.",
          parameters: memoryRetrieveParameters,
          execute: executeMemoryRetrieve,
        };

        const memoryListTool = {
          label: "Memory List",
          name: "memory_list",
          description: "List recent memories for the current user.",
          parameters: objectSchema({
            memoryType: {
              type: "string",
              description: `Optional memory type filter: ${MEMORIA_MEMORY_TYPES.join(", ")}`,
              enum: [...MEMORIA_MEMORY_TYPES],
            },
            limit: {
              type: "integer",
              description: "Maximum number of memories to return",
              minimum: 1,
              maximum: 200,
            },
            sessionId: {
              type: "string",
              description: "Optional session filter",
            },
            includeInactive: {
              type: "boolean",
              description: "Include inactive memories when the backend supports it",
            },
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const result = await client.listMemories({
              userId,
              memoryType: readMemoryType(params, "memoryType"),
              limit: clampInt(readNumber(params, "limit"), 1, 200, 20),
              sessionId: readString(params, "sessionId"),
              includeInactive: readBoolean(params, "includeInactive") ?? false,
            });
            return jsonResult({
              backend: config.backend,
              userId,
              count: result.count,
              items: result.items,
              includeInactive: result.include_inactive ?? false,
              partial: result.partial ?? false,
              limitations: result.limitations ?? [],
            });
          },
        };

        const memoryStatsTool = {
          label: "Memory Stats",
          name: "memory_stats",
          description: "Return aggregate memory statistics for the current user.",
          parameters: objectSchema({
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const stats = await client.stats(userId);
            return jsonResult(buildMemoryStatsPayload(config, userId, stats));
          },
        };

        const memoryProfileTool = {
          label: "Memory Profile",
          name: "memory_profile",
          description: "Read the Memoria profile summary for the current user.",
          parameters: objectSchema({
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const profile = await client.profile(userId);
            const summary = profile.profile?.trim() || "No profile available yet.";
            return textResult(summary, {
              profile,
            });
          },
        };

        const memoryCorrectTool = {
          label: "Memory Correct",
          name: "memory_correct",
          description: "Correct an existing memory by id or by semantic query.",
          parameters: objectSchema(
            {
              memoryId: { type: "string", description: "Specific memory id to correct" },
              query: { type: "string", description: "Semantic query used to locate the memory" },
              newContent: { type: "string", description: "Corrected memory content" },
              reason: { type: "string", description: "Optional correction reason" },
              userId: userIdProperty,
            },
            ["newContent"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const memoryId = readString(params, "memoryId");
            const query = readString(params, "query");
            const newContent = readString(params, "newContent", {
              required: true,
              label: "newContent",
            })!;
            const reason = readString(params, "reason") ?? "";
            const userId = resolveUserId(config, ctx, readString(params, "userId"));

            if (!memoryId && !query) {
              throw new Error("memoryId or query required");
            }

            const updated = memoryId
              ? await client.correctById({ userId, memoryId, newContent, reason })
              : await client.correctByQuery({ userId, query: query!, newContent, reason });

            if (hasMessageError(updated)) {
              return textResult(updated.message, {
                ok: false,
                userId,
                result: updated,
              });
            }

            return textResult(`Corrected memory ${updated.memory_id}.`, {
              ok: true,
              userId,
              memory: updated,
            });
          },
        };

        const memoryPurgeTool = {
          label: "Memory Purge",
          name: "memory_purge",
          description: "Delete memories by id or by keyword topic.",
          parameters: objectSchema({
            memoryId: { type: "string", description: "Specific memory id to delete" },
            topic: { type: "string", description: "Keyword/topic for bulk deletion" },
            reason: { type: "string", description: "Optional deletion reason" },
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const memoryId = readString(params, "memoryId");
            const topic = readString(params, "topic");
            const reason = readString(params, "reason") ?? "";
            const userId = resolveUserId(config, ctx, readString(params, "userId"));

            if (!memoryId && !topic) {
              throw new Error("memoryId or topic required");
            }

            const result = await client.purgeMemory({
              userId,
              memoryId,
              topic,
              reason,
            });

            return textResult(`Purged ${String(result.purged ?? 0)} memories.`, {
              ok: true,
              userId,
              result,
            });
          },
        };

        const memoryForgetTool = {
          label: "Memory Forget",
          name: "memory_forget",
          description: "Delete a memory by id or find one by query and delete it.",
          parameters: objectSchema({
            memoryId: { type: "string", description: "Specific memory id to delete" },
            query: { type: "string", description: "Semantic query used to locate a memory" },
            reason: { type: "string", description: "Optional deletion reason" },
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const memoryId = readString(params, "memoryId");
            const query = readString(params, "query");
            const reason = readString(params, "reason") ?? "";
            const userId = resolveUserId(config, ctx, readString(params, "userId"));

            if (!memoryId && !query) {
              throw new Error("memoryId or query required");
            }

            if (memoryId) {
              const result = await client.deleteMemory({ userId, memoryId, reason });
              return textResult(`Forgot memory ${memoryId}.`, {
                ok: true,
                userId,
                result,
              });
            }

            const candidates = await client.search({
              userId,
              query: query!,
              topK: 5,
            });

            if (candidates.length === 0) {
              return textResult("No matching memories found.", {
                ok: false,
                userId,
                candidates: [],
              });
            }

            if (candidates.length > 1) {
              return textResult(
                `Found ${candidates.length} candidates. Re-run with memoryId.\n${formatMemoryList(candidates)}`,
                {
                  ok: false,
                  userId,
                  candidates,
                },
              );
            }

            const result = await client.deleteMemory({
              userId,
              memoryId: candidates[0].memory_id,
              reason,
            });
            return textResult(`Forgot memory ${candidates[0].memory_id}.`, {
              ok: true,
              userId,
              result,
              memory: candidates[0],
            });
          },
        };

        const memoryObserveTool = {
          label: "Memory Observe",
          name: "memory_observe",
          description: "Run Memoria's observe pipeline over explicit conversation messages.",
          parameters: objectSchema(
            {
              messages: {
                type: "array",
                description: "Conversation messages as { role, content } objects",
                items: {
                  type: "object",
                  additionalProperties: false,
                  properties: {
                    role: { type: "string" },
                    content: { type: "string" },
                  },
                  required: ["role", "content"],
                },
              },
              sourceEventIds: {
                type: "array",
                description: "Optional upstream event identifiers",
                items: { type: "string" },
              },
              sessionId: {
                type: "string",
                description: "Optional session scope passed through to Memoria observe",
              },
              userId: userIdProperty,
            },
            ["messages"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const messages = readObserveMessages(params, "messages");
            const sourceEventIds = readStringList(params, "sourceEventIds");
            const created = await client.observe({
              userId,
              messages,
              sourceEventIds,
              sessionId: readString(params, "sessionId") ?? ctx.sessionId,
            });
            return jsonResult({
              ok: true,
              userId,
              count: created.length,
              memories: created,
            });
          },
        };

        const memoryGovernanceTool = {
          label: "Memory Governance",
          name: "memory_governance",
          description: "Run Memoria governance for the current user.",
          parameters: objectSchema({
            force: forceProperty,
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const result = await client.governance({
              userId,
              force: readBoolean(params, "force") ?? false,
            });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryConsolidateTool = {
          label: "Memory Consolidate",
          name: "memory_consolidate",
          description: "Run Memoria graph consolidation for the current user.",
          parameters: objectSchema({
            force: forceProperty,
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const result = await client.consolidate({
              userId,
              force: readBoolean(params, "force") ?? false,
            });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryReflectTool = {
          label: "Memory Reflect",
          name: "memory_reflect",
          description: "Run Memoria reflection or return reflection candidates.",
          parameters: objectSchema({
            mode: modeProperty,
            force: forceProperty,
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const mode = readString(params, "mode") ?? "auto";
            if (!["auto", "internal", "candidates"].includes(mode)) {
              throw new Error("mode must be one of auto, internal, candidates");
            }
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const force = readBoolean(params, "force") ?? false;
            const result = await client.reflect({ userId, force, mode });
            const payload = asRecord(result) ?? {};

            return jsonResult({
              mode,
              userId,
              ...payload,
            });
          },
        };

        const memoryExtractEntitiesTool = {
          label: "Memory Extract Entities",
          name: "memory_extract_entities",
          description: "Run Memoria entity extraction or return extraction candidates.",
          parameters: objectSchema({
            mode: modeProperty,
            force: forceProperty,
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const mode = readString(params, "mode") ?? "auto";
            if (!["auto", "internal", "candidates"].includes(mode)) {
              throw new Error("mode must be one of auto, internal, candidates");
            }
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const force = readBoolean(params, "force") ?? false;
            const result = await client.extractEntities({ userId, force, mode });
            const payload = asRecord(result) ?? {};

            return jsonResult({
              mode,
              userId,
              ...payload,
            });
          },
        };

        const memoryLinkEntitiesTool = {
          label: "Memory Link Entities",
          name: "memory_link_entities",
          description: "Write entity links from candidate extraction results.",
          parameters: objectSchema(
            {
              entities: {
                description: "Array or JSON string of [{ memory_id, entities: [{ name, type }] }]",
              },
              userId: userIdProperty,
            },
            ["entities"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const entities = readEntityPayload(params, "entities");
            const result = await client.linkEntities({ userId, entities });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryRebuildIndexTool = {
          label: "Memory Rebuild Index",
          name: "memory_rebuild_index",
          description: "Rebuild a Memoria IVF vector index.",
          parameters: objectSchema({
            table: {
              type: "string",
              description: "Target table",
              enum: ["mem_memories", "memory_graph_nodes"],
            },
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const table = readString(params, "table") ?? "mem_memories";
            if (!["mem_memories", "memory_graph_nodes"].includes(table)) {
              throw new Error("table must be one of mem_memories, memory_graph_nodes");
            }
            const result = await client.rebuildIndex(table);
            return jsonResult({
              table,
              ...((asRecord(result) ?? {}) as Record<string, unknown>),
            });
          },
        };

        const memoryCapabilitiesTool = {
          label: "Memory Capabilities",
          name: "memory_capabilities",
          description: "List tool coverage and backend-specific limitations for this plugin.",
          parameters: EMPTY_OBJECT_SCHEMA,
          execute: async () => {
            return jsonResult(buildCapabilitiesPayload(config));
          },
        };

        const memorySnapshotTool = {
          label: "Memory Snapshot",
          name: "memory_snapshot",
          description: "Create a named snapshot of current memory state.",
          parameters: objectSchema(
            {
              name: { type: "string", description: "Snapshot name" },
              description: { type: "string", description: "Optional snapshot description" },
              userId: userIdProperty,
            },
            ["name"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const name = readString(params, "name", { required: true, label: "name" })!;
            const snapshot = await client.createSnapshot({
              userId,
              name,
              description: readString(params, "description") ?? "",
            });
            return jsonResult({
              userId,
              snapshot,
            });
          },
        };

        const memorySnapshotsTool = {
          label: "Memory Snapshots",
          name: "memory_snapshots",
          description: "List all known memory snapshots.",
          parameters: objectSchema({
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const snapshots = await client.listSnapshots(userId);
            return jsonResult({
              userId,
              snapshots,
            });
          },
        };

        const memoryRollbackTool = {
          label: "Memory Rollback",
          name: "memory_rollback",
          description: "Rollback memory state to a named snapshot.",
          parameters: objectSchema(
            {
              name: { type: "string", description: "Snapshot name" },
              userId: userIdProperty,
            },
            ["name"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const name = readString(params, "name", { required: true, label: "name" })!;
            const result = await client.rollbackSnapshot({ userId, name });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryBranchTool = {
          label: "Memory Branch",
          name: "memory_branch",
          description: "Create a new memory branch for isolated experimentation.",
          parameters: objectSchema(
            {
              name: { type: "string", description: "Branch name" },
              fromSnapshot: { type: "string", description: "Optional source snapshot name" },
              fromTimestamp: {
                type: "string",
                description: "Optional source timestamp in YYYY-MM-DD HH:MM:SS",
              },
              userId: userIdProperty,
            },
            ["name"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const name = readString(params, "name", { required: true, label: "name" })!;
            const fromSnapshot = readString(params, "fromSnapshot");
            const fromTimestamp = readString(params, "fromTimestamp");
            if (fromSnapshot && fromTimestamp) {
              throw new Error("fromSnapshot and fromTimestamp are mutually exclusive");
            }
            const result = await client.branchCreate({
              userId,
              name,
              fromSnapshot,
              fromTimestamp,
            });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryBranchesTool = {
          label: "Memory Branches",
          name: "memory_branches",
          description: "List all memory branches for the current user.",
          parameters: objectSchema({
            userId: userIdProperty,
          }),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const branches = await client.branchList(userId);
            return jsonResult({
              userId,
              branches,
            });
          },
        };

        const memoryCheckoutTool = {
          label: "Memory Checkout",
          name: "memory_checkout",
          description: "Switch the active memory branch.",
          parameters: objectSchema(
            {
              name: { type: "string", description: "Branch name or main" },
              userId: userIdProperty,
            },
            ["name"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const name = readString(params, "name", { required: true, label: "name" })!;
            const result = await client.branchCheckout({ userId, name });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryBranchDeleteTool = {
          label: "Memory Branch Delete",
          name: "memory_branch_delete",
          description: "Delete a memory branch.",
          parameters: objectSchema(
            {
              name: { type: "string", description: "Branch name" },
              userId: userIdProperty,
            },
            ["name"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const name = readString(params, "name", { required: true, label: "name" })!;
            const result = await client.branchDelete({ userId, name });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryMergeTool = {
          label: "Memory Merge",
          name: "memory_merge",
          description: "Merge a branch back into main.",
          parameters: objectSchema(
            {
              source: { type: "string", description: "Branch name to merge from" },
              strategy: {
                type: "string",
                description: "append skips conflicting duplicates; replace overwrites them",
                enum: ["append", "replace"],
              },
              userId: userIdProperty,
            },
            ["source"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const source = readString(params, "source", { required: true, label: "source" })!;
            const strategy = readString(params, "strategy") ?? "append";
            if (!["append", "replace"].includes(strategy)) {
              throw new Error("strategy must be one of append, replace");
            }
            const result = await client.branchMerge({ userId, source, strategy });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        const memoryDiffTool = {
          label: "Memory Diff",
          name: "memory_diff",
          description: "Show what would change if a branch were merged into main.",
          parameters: objectSchema(
            {
              source: { type: "string", description: "Branch name to diff" },
              limit: {
                type: "integer",
                description: "Maximum number of changes to return",
                minimum: 1,
                maximum: 500,
              },
              userId: userIdProperty,
            },
            ["source"],
          ),
          execute: async (_toolCallId: string, rawParams: unknown) => {
            const params = asRecord(rawParams) ?? {};
            const userId = resolveUserId(config, ctx, readString(params, "userId"));
            const source = readString(params, "source", { required: true, label: "source" })!;
            const limit = clampInt(readNumber(params, "limit"), 1, 500, 50);
            const result = await client.branchDiff({ userId, source, limit });
            return jsonResult({
              userId,
              result,
            });
          },
        };

        return [
          memorySearchTool,
          memoryGetTool,
          memoryHealthTool,
          memoryStoreTool,
          memoryRetrieveTool,
          memoryRecallTool,
          memoryListTool,
          memoryStatsTool,
          memoryProfileTool,
          memoryCorrectTool,
          memoryPurgeTool,
          memoryForgetTool,
          memoryObserveTool,
          memoryGovernanceTool,
          memoryConsolidateTool,
          memoryReflectTool,
          memoryExtractEntitiesTool,
          memoryLinkEntitiesTool,
          memoryRebuildIndexTool,
          memoryCapabilitiesTool,
          memorySnapshotTool,
          memorySnapshotsTool,
          memoryRollbackTool,
          memoryBranchTool,
          memoryBranchesTool,
          memoryCheckoutTool,
          memoryBranchDeleteTool,
          memoryMergeTool,
          memoryDiffTool,
        ];
      },
      { names: supportedToolNames() },
    );

    api.registerCli(
      ({ program }) => {
        const memoria = program.command("memoria").description("Memoria plugin commands");
        const ltm = program
          .command("ltm")
          .description("Compatibility commands for memory-lancedb-pro style workflows");

        const printJson = (value: unknown) => {
          console.log(JSON.stringify(value, null, 2));
        };

        const resolveCliUserId = (raw: unknown, fallback = config.defaultUserId) => {
          return typeof raw === "string" && raw.trim() ? raw.trim() : fallback;
        };

        const withCliClient = <Args extends unknown[]>(
          handler: (...args: Args) => Promise<void> | void,
        ) => {
          return async (...args: Args) => {
            try {
              await handler(...args);
            } finally {
              client.close();
            }
          };
        };

        memoria
          .command("health")
          .description("Check Memoria connectivity")
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.health(userId);
            printJson({
              userId,
              backend: config.backend,
              ...(asRecord(result) ?? {}),
            });
          }));

        memoria
          .command("search")
          .description("Search Memoria memories")
          .argument("<query>", "Search query")
          .option("--top-k <n>", "Maximum result count", String(config.retrieveTopK))
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .action(withCliClient(async (query, opts) => {
            const topK = clampInt(Number.parseInt(String(opts.topK), 10), 1, 20, config.retrieveTopK);
            const userId = resolveCliUserId(opts.userId);
            const result = await client.retrieve({
              userId,
              query: String(query),
              topK,
              includeCrossSession: config.includeCrossSession,
            });
            printJson({
              backend: config.backend,
              userId,
              count: result.length,
              memories: result,
            });
          }));

        memoria
          .command("list")
          .description("List recent Memoria memories")
          .option("--limit <n>", "Maximum result count", "20")
          .option("--type <memoryType>", "Optional memory type filter")
          .option("--session-id <id>", "Optional session filter")
          .option("--include-inactive", "Include inactive memories when supported", false)
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.listMemories({
              userId,
              memoryType:
                typeof opts.type === "string" && opts.type.trim()
                  ? readMemoryType({ memoryType: opts.type }, "memoryType")
                  : undefined,
              limit: clampInt(Number.parseInt(String(opts.limit), 10), 1, 200, 20),
              sessionId:
                typeof opts.sessionId === "string" && opts.sessionId.trim()
                  ? opts.sessionId.trim()
                  : undefined,
              includeInactive: Boolean(opts.includeInactive),
            });
            printJson({
              backend: config.backend,
              userId,
              count: result.count,
              items: result.items,
              includeInactive: result.include_inactive ?? false,
              partial: result.partial ?? false,
              limitations: result.limitations ?? [],
            });
          }));

        memoria
          .command("stats")
          .description("Show aggregate Memoria statistics")
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.stats(userId);
            printJson(buildMemoryStatsPayload(config, userId, result));
          }));

        memoria
          .command("profile")
          .description("Show the current Memoria profile")
          .option(
            "--user-id <user>",
            "Explicit Memoria user_id",
            config.defaultUserId,
          )
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId, config.defaultUserId);
            const result = await client.profile(userId);
            printJson(result);
          }));

        memoria
          .command("capabilities")
          .description("Show plugin capabilities and compatibility mappings")
          .action(withCliClient(async () => {
            printJson(buildCapabilitiesPayload(config));
          }));

        memoria
          .command("setup")
          .description("Configure Memoria cloud API backend")
          .option("--mode <mode>", "Backend mode (only 'cloud' is supported)", "cloud")
          .option("--api-url <url>", "Memoria API URL")
          .option("--api-key <token>", "Memoria API token")
          .option("--user-id <user>", "Default Memoria user id")
          .action(withCliClient(async (opts) => {
            const mode = typeof opts.mode === "string" ? opts.mode.trim().toLowerCase() : "cloud";
            if (mode !== "cloud") {
              throw new Error("Only cloud mode is supported. Example: openclaw memoria setup --mode cloud --api-url <URL> --api-key <KEY>");
            }
            const apiUrl = typeof opts.apiUrl === "string" ? opts.apiUrl.trim().replace(/\/+$/, "") : "";
            const apiKey = typeof opts.apiKey === "string" ? opts.apiKey.trim() : "";
            if (!apiUrl || !apiKey) {
              throw new Error("--api-url and --api-key are required. Example: openclaw memoria setup --mode cloud --api-url <URL> --api-key <KEY>");
            }
            const userId = typeof opts.userId === "string" && opts.userId.trim() ? opts.userId.trim() : undefined;
            const configFile = resolveOpenClawConfigFile();
            const fs = await import("node:fs");
            let data: Record<string, unknown> = {};
            if (fs.existsSync(configFile)) {
              data = JSON.parse(fs.readFileSync(configFile, "utf8"));
            } else {
              const dir = path.dirname(configFile);
              fs.mkdirSync(dir, { recursive: true });
            }
            const plugins = (data.plugins ?? {}) as Record<string, unknown>;
            const entries = (plugins.entries ?? {}) as Record<string, unknown>;
            const entry = (entries.thememoria ?? {}) as Record<string, unknown>;
            const pluginConfig = (entry.config ?? {}) as Record<string, unknown>;
            pluginConfig.backend = "api";
            pluginConfig.apiUrl = apiUrl;
            pluginConfig.apiKey = apiKey;
            if (userId) pluginConfig.defaultUserId = userId;
            entry.enabled = true;
            entry.config = pluginConfig;
            entries.thememoria = entry;
            plugins.entries = entries;
            const slots = (plugins.slots ?? {}) as Record<string, unknown>;
            slots.memory = "thememoria";
            plugins.slots = slots;
            data.plugins = plugins;
            fs.writeFileSync(configFile, `${JSON.stringify(data, null, 2)}\n`, "utf8");
            printJson({ ok: true, configFile, apiUrl, apiKeySet: true });
          }));

        ltm
          .command("list")
          .description("Compatibility alias for memoria list")
          .option("--limit <n>", "Maximum result count", "20")
          .option("--type <memoryType>", "Optional memory type filter")
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .option("--json", "Ignored compatibility flag; output is already JSON", true)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.listMemories({
              userId,
              memoryType:
                typeof opts.type === "string" && opts.type.trim()
                  ? readMemoryType({ memoryType: opts.type }, "memoryType")
                  : undefined,
              limit: clampInt(Number.parseInt(String(opts.limit), 10), 1, 200, 20),
            });
            printJson({
              backend: config.backend,
              userId,
              count: result.count,
              items: result.items,
              partial: result.partial ?? false,
              limitations: result.limitations ?? [],
            });
          }));

        ltm
          .command("search")
          .description("Compatibility alias for memory_recall")
          .argument("<query>", "Recall query")
          .option("--limit <n>", "Maximum result count", String(config.retrieveTopK))
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .option("--json", "Ignored compatibility flag; output is already JSON", true)
          .action(withCliClient(async (query, opts) => {
            const topK = clampInt(Number.parseInt(String(opts.limit), 10), 1, 20, config.retrieveTopK);
            const userId = resolveCliUserId(opts.userId);
            const result = await client.retrieve({
              userId,
              query: String(query),
              topK,
              includeCrossSession: config.includeCrossSession,
            });
            printJson({
              backend: config.backend,
              userId,
              count: result.length,
              memories: result,
            });
          }));

        ltm
          .command("stats")
          .description("Compatibility alias for memory_stats")
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .option("--json", "Ignored compatibility flag; output is already JSON", true)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.stats(userId);
            printJson(buildMemoryStatsPayload(config, userId, result));
          }));

        ltm
          .command("health")
          .description("Check Memoria connectivity through the compatibility CLI")
          .option("--user-id <user>", "Explicit Memoria user_id", config.defaultUserId)
          .option("--json", "Ignored compatibility flag; output is already JSON", true)
          .action(withCliClient(async (opts) => {
            const userId = resolveCliUserId(opts.userId);
            const result = await client.health(userId);
            printJson({
              userId,
              backend: config.backend,
              ...(asRecord(result) ?? {}),
            });
          }));
      },
      { commands: [...CLI_COMMAND_NAMES] },
    );

    const handleAutoRecall = async (
      prompt: string,
      ctx: PluginIdentityContext,
    ): Promise<{ appendSystemContext?: string } | void> => {
      const trimmed = prompt.trim();
      if (trimmed.length < config.recallMinPromptLength) {
        return;
      }

      const userId = resolveUserId(config, ctx);

      try {
        const memories = await client.retrieve({
          userId,
          query: trimmed,
          topK: config.retrieveTopK,
          memoryTypes: config.retrieveMemoryTypes,
          sessionId: ctx.sessionId,
          includeCrossSession: config.includeCrossSession,
        });
        if (memories.length === 0) {
          return;
        }
        api.logger.info(`thememoria: recalled ${memories.length} memories`);
        return {
          appendSystemContext: formatRelevantMemoriesContext(memories),
        };
      } catch (error) {
        api.logger.warn(`thememoria: auto-recall failed: ${String(error)}`);
      }
    };

    if (config.autoRecall) {
      api.on("before_prompt_build", async (event, ctx) => {
        return await handleAutoRecall(event.prompt, ctx);
      });
    }

    if (config.autoObserve) {
      api.on("agent_end", async (event, ctx) => {
        if (!event.success || !Array.isArray(event.messages) || event.messages.length === 0) {
          return;
        }
        const messages = collectRecentConversationMessages(event.messages, {
          tailMessages: config.observeTailMessages,
          maxChars: config.observeMaxChars,
        });
        if (messages.length === 0) {
          return;
        }

        const userId = resolveUserId(config, ctx);

        try {
          const created = await client.observe({
            userId,
            messages,
            sourceEventIds: ctx.sessionId ? [`openclaw:${ctx.sessionId}`] : undefined,
            sessionId: ctx.sessionId,
          });
          if (created.length > 0) {
            api.logger.info(`thememoria: observed ${created.length} new memories`);
          }
        } catch (error) {
          api.logger.warn(`thememoria: auto-observe failed: ${String(error)}`);
        }
      });

      api.on("before_reset", async (event, ctx) => {
        if (!Array.isArray(event.messages) || event.messages.length === 0) {
          return;
        }

        const messages = collectRecentConversationMessages(event.messages, {
          tailMessages: config.observeTailMessages,
          maxChars: config.observeMaxChars,
        });
        if (messages.length === 0) {
          return;
        }

        const userId = resolveUserId(config, ctx);

        try {
          const created = await client.observe({
            userId,
            messages,
            sourceEventIds: ctx.sessionId ? [`openclaw:before_reset:${ctx.sessionId}`] : undefined,
            sessionId: ctx.sessionId,
          });
          if (created.length > 0) {
            api.logger.info(
              `thememoria: observed ${created.length} new memories before reset`,
            );
          }
        } catch (error) {
          api.logger.warn(`thememoria: before_reset observe failed: ${String(error)}`);
        }
      });
    }

    api.on("after_compaction", async () => {
      api.logger.info(
        "thememoria: compaction finished; next prompt will use live Memoria recall",
      );
    });

    api.registerService({
      id: "thememoria",
      async start() {
        try {
          const result = await client.health(config.defaultUserId);
          api.logger.info(`thememoria: connected (${String(result.status ?? "ok")})`);
        } catch (error) {
          api.logger.warn(`thememoria: health check failed: ${String(error)}`);
        }
      },
      stop() {
        client.close();
        api.logger.info("thememoria: stopped");
      },
    });
  },
};

export default plugin;
