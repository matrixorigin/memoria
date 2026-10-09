import type { OpenClawPluginConfigSchema } from "openclaw/plugin-sdk";

export const MEMORIA_MEMORY_TYPES = [
  "profile",
  "semantic",
  "procedural",
  "working",
  "tool_result",
] as const;

export const MEMORIA_TRUST_TIERS = ["T1", "T2", "T3", "T4"] as const;
export const MEMORIA_BACKENDS = ["embedded", "api"] as const;
export const MEMORIA_USER_ID_STRATEGIES = ["config", "agentId", "sessionKey"] as const;

export type MemoriaMemoryType = (typeof MEMORIA_MEMORY_TYPES)[number];
export type MemoriaTrustTier = (typeof MEMORIA_TRUST_TIERS)[number];
export type MemoriaBackendMode = (typeof MEMORIA_BACKENDS)[number];
export type MemoriaUserIdStrategy = (typeof MEMORIA_USER_ID_STRATEGIES)[number];

export type MemoriaPluginConfig = {
  backend: MemoriaBackendMode;
  dbUrl: string;
  apiUrl?: string;
  apiKey?: string;
  memoriaExecutable: string;
  defaultUserId: string;
  userIdStrategy: MemoriaUserIdStrategy;
  timeoutMs: number;
  maxListPages: number;
  autoRecall: boolean;
  autoObserve: boolean;
  retrieveTopK: number;
  recallMinPromptLength: number;
  includeCrossSession: boolean;
  retrieveMemoryTypes?: MemoriaMemoryType[];
  observeTailMessages: number;
  observeMaxChars: number;
  embeddingProvider: string;
  embeddingModel: string;
  embeddingBaseUrl?: string;
  embeddingApiKey?: string;
  embeddingDim?: number;
  llmApiKey?: string;
  llmBaseUrl?: string;
  llmModel?: string;
};

type Issue = { path: Array<string | number>; message: string };
type SafeParseResult =
  | { success: true; data: MemoriaPluginConfig }
  | { success: false; error: { issues: Issue[] } };

const DEFAULTS = {
  backend: "embedded" as MemoriaBackendMode,
  dbUrl: "mysql://root:111@127.0.0.1:6001/memoria",
  apiUrl: "http://127.0.0.1:8100",
  memoriaExecutable: "memoria",
  defaultUserId: "openclaw-user",
  userIdStrategy: "config" as MemoriaUserIdStrategy,
  timeoutMs: 15_000,
  maxListPages: 20,
  autoRecall: true,
  autoObserve: false,
  retrieveTopK: 5,
  recallMinPromptLength: 8,
  includeCrossSession: true,
  observeTailMessages: 6,
  observeMaxChars: 6_000,
  embeddingProvider: "openai",
  embeddingModel: "text-embedding-3-small",
  llmModel: "gpt-4o-mini",
} as const;

const UI_HINTS: Record<
  string,
  {
    label?: string;
    help?: string;
    tags?: string[];
    advanced?: boolean;
    sensitive?: boolean;
    placeholder?: string;
  }
> = {
  backend: {
    label: "Backend Mode",
    help: "embedded runs the Rust memoria CLI locally against MatrixOne; api connects directly to the Memoria REST API over HTTP.",
    placeholder: DEFAULTS.backend,
  },
  dbUrl: {
    label: "MatrixOne Connection String",
    help: "Embedded mode only. Rust Memoria expects a MySQL DSN; old mysql+pymysql:// values are normalized automatically.",
    placeholder: DEFAULTS.dbUrl,
  },
  apiUrl: {
    label: "Memoria API URL",
    help: "Only used when backend=api.",
    placeholder: DEFAULTS.apiUrl,
  },
  apiKey: {
    label: "Memoria API Token",
    help: "Bearer token for backend=api.",
    sensitive: true,
    placeholder: "sk-...",
  },
  memoriaExecutable: {
    label: "Memoria Executable",
    help: "Path or command name for the Rust memoria CLI.",
    advanced: true,
    placeholder: DEFAULTS.memoriaExecutable,
  },
  defaultUserId: {
    label: "Default User ID",
    help: "Single-user quick-start identity used unless userIdStrategy derives one from the OpenClaw runtime context.",
    placeholder: DEFAULTS.defaultUserId,
  },
  userIdStrategy: {
    label: "User ID Strategy",
    help: "config keeps one shared Memoria user; agentId or sessionKey derive the identity from OpenClaw runtime context.",
    advanced: true,
    placeholder: DEFAULTS.userIdStrategy,
  },
  timeoutMs: {
    label: "Timeout",
    help: "Timeout for Memoria CLI requests in milliseconds.",
    advanced: true,
    placeholder: String(DEFAULTS.timeoutMs),
  },
  maxListPages: {
    label: "List Page Limit",
    help: "Maximum number of API pages (up to 500 memories each) fetched per memory_list call, which also bounds derived stats and client-side type filtering.",
    advanced: true,
    placeholder: String(DEFAULTS.maxListPages),
  },
  autoRecall: {
    label: "Auto-Recall",
    help: "Automatically inject relevant memories into the prompt before each run.",
  },
  autoObserve: {
    label: "Auto-Observe",
    help: "Automatically extract memories from recent conversation turns at agent_end.",
  },
  retrieveTopK: {
    label: "Recall Top K",
    help: "Maximum number of memories returned for memory_search and auto-recall.",
    placeholder: String(DEFAULTS.retrieveTopK),
  },
  recallMinPromptLength: {
    label: "Recall Min Length",
    help: "Prompts shorter than this are skipped for auto-recall.",
    advanced: true,
    placeholder: String(DEFAULTS.recallMinPromptLength),
  },
  includeCrossSession: {
    label: "Cross-Session Recall",
    help: "Compatibility flag retained from the Python plugin. Rust MCP currently uses the upstream default retrieval behavior.",
    advanced: true,
  },
  retrieveMemoryTypes: {
    label: "Memory Types",
    help: "Compatibility hint retained from the Python plugin. Rust MCP currently ignores this filter.",
    advanced: true,
  },
  observeTailMessages: {
    label: "Observe Tail Messages",
    help: "Number of recent user/assistant messages forwarded to memory_observe.",
    advanced: true,
    placeholder: String(DEFAULTS.observeTailMessages),
  },
  observeMaxChars: {
    label: "Observe Max Chars",
    help: "Maximum total characters forwarded to memory_observe.",
    advanced: true,
    placeholder: String(DEFAULTS.observeMaxChars),
  },
  embeddingProvider: {
    label: "Embedding Provider",
    help: "Embedded mode only. Prebuilt Rust release binaries are typically used with openai-compatible embedding services.",
    placeholder: DEFAULTS.embeddingProvider,
  },
  embeddingModel: {
    label: "Embedding Model",
    help: "Embedded mode only.",
    placeholder: DEFAULTS.embeddingModel,
  },
  embeddingBaseUrl: {
    label: "Embedding Base URL",
    help: "Embedded mode only. Optional for official OpenAI; required for compatible gateways.",
    advanced: true,
    placeholder: "https://api.openai.com/v1",
  },
  embeddingApiKey: {
    label: "Embedding API Key",
    help: "Embedded mode only. Required for OpenAI-compatible providers unless you built Memoria with local-embedding.",
    advanced: true,
    sensitive: true,
    placeholder: "sk-...",
  },
  embeddingDim: {
    label: "Embedding Dimensions",
    help: "Embedded mode only. Set this before first startup so Memoria creates the right schema.",
    advanced: true,
    placeholder: "1536",
  },
  llmApiKey: {
    label: "Observer LLM API Key",
    help: "Optional. Enables auto-observe extraction and internal LLM-backed Memoria tools.",
    advanced: true,
    sensitive: true,
    placeholder: "sk-...",
  },
  llmBaseUrl: {
    label: "Observer LLM Base URL",
    help: "Optional OpenAI-compatible base URL for embedded auto-observe and reflection flows.",
    advanced: true,
    placeholder: "https://api.openai.com/v1",
  },
  llmModel: {
    label: "Observer LLM Model",
    help: "Model used by embedded auto-observe and internal reflection/entity extraction.",
    advanced: true,
    placeholder: DEFAULTS.llmModel,
  },
};

export const memoriaPluginJsonSchema: Record<string, unknown> = {
  type: "object",
  additionalProperties: false,
  properties: {
    backend: {
      type: "string",
      enum: [...MEMORIA_BACKENDS],
    },
    dbUrl: {
      type: "string",
    },
    apiUrl: {
      type: "string",
    },
    apiKey: {
      type: "string",
    },
    memoriaExecutable: {
      type: "string",
    },
    // Legacy keys retained for config compatibility during migration.
    pythonExecutable: {
      type: "string",
    },
    memoriaRoot: {
      type: "string",
    },
    defaultUserId: {
      type: "string",
    },
    userIdStrategy: {
      type: "string",
      enum: [...MEMORIA_USER_ID_STRATEGIES],
    },
    timeoutMs: {
      type: "integer",
      minimum: 1000,
      maximum: 120000,
    },
    maxListPages: {
      type: "integer",
      minimum: 1,
      maximum: 100,
    },
    autoRecall: {
      type: "boolean",
    },
    autoObserve: {
      type: "boolean",
    },
    retrieveTopK: {
      type: "integer",
      minimum: 1,
      maximum: 20,
    },
    recallMinPromptLength: {
      type: "integer",
      minimum: 1,
      maximum: 500,
    },
    includeCrossSession: {
      type: "boolean",
    },
    retrieveMemoryTypes: {
      type: "array",
      items: {
        type: "string",
        enum: [...MEMORIA_MEMORY_TYPES],
      },
    },
    observeTailMessages: {
      type: "integer",
      minimum: 2,
      maximum: 30,
    },
    observeMaxChars: {
      type: "integer",
      minimum: 256,
      maximum: 50000,
    },
    embeddingProvider: {
      type: "string",
    },
    embeddingModel: {
      type: "string",
    },
    embeddingBaseUrl: {
      type: "string",
    },
    embeddingApiKey: {
      type: "string",
    },
    embeddingDim: {
      type: "integer",
      minimum: 1,
    },
    llmApiKey: {
      type: "string",
    },
    llmBaseUrl: {
      type: "string",
    },
    llmModel: {
      type: "string",
    },
  },
};

function fail(message: string, path: Array<string | number> = []): never {
  const error = new Error(message) as Error & { issues?: Issue[] };
  error.issues = [{ path, message }];
  throw error;
}

function asObject(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    fail("expected config object");
  }
  return value as Record<string, unknown>;
}

function resolveEnvVars(value: string): string {
  return value.replace(/\$\{([^}]+)\}/g, (_match, envVar: string) => {
    const resolved = process.env[envVar];
    if (!resolved) {
      fail(`environment variable ${envVar} is not set`);
    }
    return resolved;
  });
}

function normalizeDbUrl(value: string): string {
  return value.replace(/^mysql\+pymysql:\/\//, "mysql://");
}

function readString(
  input: Record<string, unknown>,
  key: string,
  options: {
    required?: boolean;
    defaultValue?: string;
    trim?: boolean;
  } = {},
): string {
  const { required = false, defaultValue, trim = true } = options;
  const raw = input[key];
  if (raw === undefined || raw === null || raw === "") {
    if (defaultValue !== undefined) {
      return defaultValue;
    }
    if (required) {
      fail(`${key} required`, [key]);
    }
    return "";
  }
  if (typeof raw !== "string") {
    fail(`${key} must be a string`, [key]);
  }
  const value = trim ? raw.trim() : raw;
  if (!value) {
    if (defaultValue !== undefined) {
      return defaultValue;
    }
    if (required) {
      fail(`${key} required`, [key]);
    }
    return "";
  }
  return resolveEnvVars(value);
}

function readBoolean(
  input: Record<string, unknown>,
  key: string,
  defaultValue: boolean,
): boolean {
  const raw = input[key];
  if (raw === undefined) {
    return defaultValue;
  }
  if (typeof raw !== "boolean") {
    fail(`${key} must be a boolean`, [key]);
  }
  return raw;
}

function readInteger(
  input: Record<string, unknown>,
  key: string,
  defaultValue: number,
  min: number,
  max: number,
): number {
  const raw = input[key];
  if (raw === undefined) {
    return defaultValue;
  }
  if (typeof raw !== "number" || !Number.isFinite(raw) || !Number.isInteger(raw)) {
    fail(`${key} must be an integer`, [key]);
  }
  if (raw < min || raw > max) {
    fail(`${key} must be between ${min} and ${max}`, [key]);
  }
  return raw;
}

function readEnum<T extends readonly string[]>(
  input: Record<string, unknown>,
  key: string,
  values: T,
  defaultValue: T[number],
): T[number] {
  const value = readString(input, key, { defaultValue });
  if (!values.includes(value as T[number])) {
    fail(`${key} must be one of ${values.join(", ")}`, [key]);
  }
  return value as T[number];
}

function readMemoryTypes(
  input: Record<string, unknown>,
  key: string,
): MemoriaMemoryType[] | undefined {
  const raw = input[key];
  if (raw === undefined) {
    return undefined;
  }
  if (!Array.isArray(raw)) {
    fail(`${key} must be an array`, [key]);
  }
  const values = raw.map((entry, index) => {
    if (typeof entry !== "string") {
      fail(`${key}[${index}] must be a string`, [key, index]);
    }
    const normalized = entry.trim();
    if (!MEMORIA_MEMORY_TYPES.includes(normalized as MemoriaMemoryType)) {
      fail(`${key}[${index}] must be one of ${MEMORIA_MEMORY_TYPES.join(", ")}`, [key, index]);
    }
    return normalized as MemoriaMemoryType;
  });
  return values.length > 0 ? values : undefined;
}

function assertNoUnknownKeys(input: Record<string, unknown>) {
  const allowed = new Set([
    "backend",
    "dbUrl",
    "apiUrl",
    "apiKey",
    "memoriaExecutable",
    "pythonExecutable",
    "memoriaRoot",
    "defaultUserId",
    "userIdStrategy",
    "timeoutMs",
    "maxListPages",
    "autoRecall",
    "autoObserve",
    "retrieveTopK",
    "recallMinPromptLength",
    "includeCrossSession",
    "retrieveMemoryTypes",
    "observeTailMessages",
    "observeMaxChars",
    "embeddingProvider",
    "embeddingModel",
    "embeddingBaseUrl",
    "embeddingApiKey",
    "embeddingDim",
    "llmApiKey",
    "llmBaseUrl",
    "llmModel",
  ]);
  for (const key of Object.keys(input)) {
    if (!allowed.has(key)) {
      fail(`unknown config key: ${key}`, [key]);
    }
  }
}

function optional(value: string): string | undefined {
  return value.trim() ? value.trim() : undefined;
}

export function parseMemoriaPluginConfig(value: unknown): MemoriaPluginConfig {
  const input = asObject(value ?? {});
  assertNoUnknownKeys(input);

  // Reject legacy "http" backend — it was removed in favor of "api"
  if (typeof input.backend === "string" && input.backend.trim().toLowerCase() === "http") {
    fail(
      "backend 'http' is no longer supported. Use 'api' for cloud mode (direct HTTP to Memoria REST API, no binary needed).",
      ["backend"],
    );
  }

  const backend = readEnum(input, "backend", MEMORIA_BACKENDS, DEFAULTS.backend);
  const apiUrl = optional(readString(input, "apiUrl", { defaultValue: DEFAULTS.apiUrl }))?.replace(
    /\/+$/,
    "",
  );
  const apiKey = optional(readString(input, "apiKey"));
  if (backend === "api") {
    if (!apiUrl) {
      fail("apiUrl required when backend=api", ["apiUrl"]);
    }
    if (!apiKey) {
      fail("apiKey required when backend=api", ["apiKey"]);
    }
  }

  const embeddingBaseUrl = optional(readString(input, "embeddingBaseUrl"))?.replace(/\/+$/, "");
  const embeddingApiKey = optional(readString(input, "embeddingApiKey"));
  const llmApiKey = optional(readString(input, "llmApiKey"));
  const llmBaseUrl = optional(readString(input, "llmBaseUrl"))?.replace(/\/+$/, "");
  const llmModel = optional(readString(input, "llmModel", { defaultValue: DEFAULTS.llmModel }));

  const embeddingDimRaw = input.embeddingDim;
  let embeddingDim: number | undefined;
  if (embeddingDimRaw !== undefined) {
    if (
      typeof embeddingDimRaw !== "number" ||
      !Number.isFinite(embeddingDimRaw) ||
      !Number.isInteger(embeddingDimRaw) ||
      embeddingDimRaw < 1
    ) {
      fail("embeddingDim must be a positive integer", ["embeddingDim"]);
    }
    embeddingDim = embeddingDimRaw;
  }

  return {
    backend,
    dbUrl: normalizeDbUrl(readString(input, "dbUrl", { defaultValue: DEFAULTS.dbUrl })),
    apiUrl,
    apiKey,
    memoriaExecutable: readString(input, "memoriaExecutable", {
      defaultValue: DEFAULTS.memoriaExecutable,
    }),
    defaultUserId: readString(input, "defaultUserId", {
      defaultValue: DEFAULTS.defaultUserId,
    }),
    userIdStrategy: readEnum(
      input,
      "userIdStrategy",
      MEMORIA_USER_ID_STRATEGIES,
      DEFAULTS.userIdStrategy,
    ),
    timeoutMs: readInteger(input, "timeoutMs", DEFAULTS.timeoutMs, 1_000, 120_000),
    maxListPages: readInteger(input, "maxListPages", DEFAULTS.maxListPages, 1, 100),
    autoRecall: readBoolean(input, "autoRecall", DEFAULTS.autoRecall),
    autoObserve: readBoolean(input, "autoObserve", DEFAULTS.autoObserve),
    retrieveTopK: readInteger(input, "retrieveTopK", DEFAULTS.retrieveTopK, 1, 20),
    recallMinPromptLength: readInteger(
      input,
      "recallMinPromptLength",
      DEFAULTS.recallMinPromptLength,
      1,
      500,
    ),
    includeCrossSession: readBoolean(
      input,
      "includeCrossSession",
      DEFAULTS.includeCrossSession,
    ),
    retrieveMemoryTypes: readMemoryTypes(input, "retrieveMemoryTypes"),
    observeTailMessages: readInteger(
      input,
      "observeTailMessages",
      DEFAULTS.observeTailMessages,
      2,
      30,
    ),
    observeMaxChars: readInteger(
      input,
      "observeMaxChars",
      DEFAULTS.observeMaxChars,
      256,
      50_000,
    ),
    embeddingProvider: readString(input, "embeddingProvider", {
      defaultValue: DEFAULTS.embeddingProvider,
    }),
    embeddingModel: readString(input, "embeddingModel", {
      defaultValue: DEFAULTS.embeddingModel,
    }),
    embeddingBaseUrl,
    embeddingApiKey,
    embeddingDim,
    llmApiKey,
    llmBaseUrl,
    llmModel,
  };
}

export const memoriaPluginConfigSchema: OpenClawPluginConfigSchema = {
  parse(value: unknown) {
    return parseMemoriaPluginConfig(value);
  },
  safeParse(value: unknown): SafeParseResult {
    try {
      return { success: true, data: parseMemoriaPluginConfig(value) };
    } catch (error) {
      const issues = (error as Error & { issues?: Issue[] }).issues ?? [
        { path: [], message: (error as Error).message },
      ];
      return { success: false, error: { issues } };
    }
  },
  jsonSchema: memoriaPluginJsonSchema,
  uiHints: UI_HINTS,
};
