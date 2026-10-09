/**
 * Shared test helpers: config builders, fetch mocks, sample data.
 */
import { vi } from "vitest";
import type { MemoriaPluginConfig } from "../config.js";

// ── Config builders ──────────────────────────────────────────

const BASE_DEFAULTS: MemoriaPluginConfig = {
  backend: "embedded",
  dbUrl: "mysql://root:111@127.0.0.1:6001/memoria",
  memoriaExecutable: "memoria",
  defaultUserId: "openclaw-user",
  userIdStrategy: "config",
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
};

export function buildApiConfig(overrides: Partial<MemoriaPluginConfig> = {}): MemoriaPluginConfig {
  return {
    ...BASE_DEFAULTS,
    backend: "api",
    apiUrl: "https://memoria.example.com",
    apiKey: "sk-test-key-123",
    ...overrides,
  };
}

export function buildEmbeddedConfig(overrides: Partial<MemoriaPluginConfig> = {}): MemoriaPluginConfig {
  return { ...BASE_DEFAULTS, ...overrides };
}

// ── Fetch mock ───────────────────────────────────────────────

export type MockFetchCall = {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: unknown;
};

/**
 * Install a mock for globalThis.fetch that records calls and returns
 * configurable responses. Returns helpers to inspect calls and set responses.
 */
export function mockFetch() {
  const calls: MockFetchCall[] = [];
  let nextResponse: { status: number; body: unknown; headers?: Record<string, string> } = {
    status: 200,
    body: { ok: true },
  };
  let handler: ((url: string) => { status: number; body: unknown }) | null = null;

  const mock = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const urlStr = typeof url === "string" ? url : url instanceof URL ? url.toString() : url.url;
    const method = init?.method ?? "GET";
    const hdrs: Record<string, string> = {};
    if (init?.headers) {
      if (init.headers instanceof Headers) {
        init.headers.forEach((v, k) => { hdrs[k] = v; });
      } else if (Array.isArray(init.headers)) {
        for (const [k, v] of init.headers) hdrs[k] = v;
      } else {
        Object.assign(hdrs, init.headers);
      }
    }
    let body: unknown = undefined;
    if (init?.body && typeof init.body === "string") {
      try { body = JSON.parse(init.body); } catch { body = init.body; }
    }
    calls.push({ url: urlStr, method, headers: hdrs, body });
    if (handler) {
      nextResponse = handler(urlStr);
    }

    const responseBody = typeof nextResponse.body === "string"
      ? nextResponse.body
      : JSON.stringify(nextResponse.body);

    return new Response(responseBody, {
      status: nextResponse.status,
      headers: { "Content-Type": "application/json", ...nextResponse.headers },
    });
  });

  globalThis.fetch = mock as typeof globalThis.fetch;

  return {
    calls,
    mock,
    /** Set what the next fetch call will return */
    respondWith(status: number, body: unknown) {
      handler = null;
      nextResponse = { status, body };
    },
    /** Compute each response from the request URL (e.g. to serve paginated pages) */
    respondWithHandler(fn: (url: string) => { status: number; body: unknown }) {
      handler = fn;
    },
    /** Get the last recorded call */
    lastCall(): MockFetchCall | undefined {
      return calls[calls.length - 1];
    },
    /** Reset recorded calls */
    reset() {
      calls.length = 0;
    },
  };
}

// ── Sample data ──────────────────────────────────────────────

export const SAMPLE_MEMORY = {
  memory_id: "abc-123",
  content: "User prefers dark mode",
  memory_type: "profile",
  trust_tier: "T1",
  confidence: 0.85,
};

export const SAMPLE_MEMORIES_API_RESPONSE = {
  results: [
    { memory_id: "m1", content: "First memory", memory_type: "semantic" },
    { memory_id: "m2", content: "Second memory", memory_type: "profile", confidence: 0.9 },
  ],
};
