/**
 * Group 3: Cloud API Communication
 * "Does the plugin talk to Memoria API correctly?"
 *
 * Bug 2 regression: no X-User-Id header should be sent (API key determines identity).
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, mockFetch, type MockFetchCall } from "./helpers.js";

let fetchMock: ReturnType<typeof mockFetch>;
let transport: MemoriaHttpTransport;
const originalFetch = globalThis.fetch;

beforeEach(() => {
  fetchMock = mockFetch();
  transport = new MemoriaHttpTransport(buildApiConfig(), "test-user");
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe("Group 3: Cloud API Communication", () => {
  // ── Constructor & lifecycle ──────────────────────────────

  describe("Constructor & lifecycle", () => {
    it("3.1 missing apiUrl throws", () => {
      expect(() => new MemoriaHttpTransport(
        buildApiConfig({ apiUrl: undefined }), "u",
      )).toThrow(/apiUrl/);
    });

    it("3.2 missing apiKey throws", () => {
      expect(() => new MemoriaHttpTransport(
        buildApiConfig({ apiKey: undefined }), "u",
      )).toThrow(/apiKey/);
    });

    it("3.3 trailing slash stripped from apiUrl", async () => {
      const t = new MemoriaHttpTransport(
        buildApiConfig({ apiUrl: "https://example.com///" }), "u",
      );
      fetchMock.respondWith(200, { status: "ok" });
      await t.healthCheck();
      expect(fetchMock.lastCall()!.url).toMatch(/^https:\/\/example\.com\/health/);
    });

    it("3.4 isAlive() always returns true", () => {
      expect(transport.isAlive()).toBe(true);
    });

    it("3.5 close() is a no-op", () => {
      expect(() => transport.close()).not.toThrow();
    });
  });

  // ── Auth & headers (Bug 2 regression) ────────────────────

  describe("Auth & headers", () => {
    it("3.6 every request has Authorization: Bearer <key>", async () => {
      fetchMock.respondWith(200, { results: [] });
      await transport.callTool("memory_retrieve", { query: "test", top_k: 5 });
      expect(fetchMock.lastCall()!.headers["Authorization"]).toBe("Bearer sk-test-key-123");
    });

    it("3.7 every request has Content-Type: application/json", async () => {
      fetchMock.respondWith(200, { results: [] });
      await transport.callTool("memory_retrieve", { query: "test", top_k: 5 });
      expect(fetchMock.lastCall()!.headers["Content-Type"]).toBe("application/json");
    });

    it("3.8 NO X-User-Id header sent (Bug 2 fix)", async () => {
      fetchMock.respondWith(200, { results: [] });
      await transport.callTool("memory_retrieve", { query: "test", top_k: 5 });
      expect(fetchMock.lastCall()!.headers["X-User-Id"]).toBeUndefined();
    });

    it("3.8b every request has X-Memoria-Tool: openclaw", async () => {
      fetchMock.respondWith(200, { results: [] });
      await transport.callTool("memory_retrieve", { query: "test", top_k: 5 });
      expect(fetchMock.lastCall()!.headers["X-Memoria-Tool"]).toBe("openclaw");
    });
  });

  // ── Tool → endpoint mapping (table-driven) ──────────────

  describe("Tool → endpoint mapping", () => {
    const cases: Array<{
      id: string;
      tool: string;
      args: Record<string, unknown>;
      method: string;
      pathPattern: RegExp;
    }> = [
      { id: "3.9", tool: "memory_store", args: { content: "hi" }, method: "POST", pathPattern: /\/v1\/memories$/ },
      { id: "3.10", tool: "memory_retrieve", args: { query: "q" }, method: "POST", pathPattern: /\/v1\/memories\/retrieve$/ },
      { id: "3.11", tool: "memory_search", args: { query: "q" }, method: "POST", pathPattern: /\/v1\/memories\/search$/ },
      { id: "3.12", tool: "memory_list", args: { limit: 10 }, method: "GET", pathPattern: /\/v1\/memories\?limit=10/ },
      { id: "3.13", tool: "memory_profile", args: {}, method: "GET", pathPattern: /\/v1\/profiles\/me$/ },
      { id: "3.14", tool: "memory_correct", args: { memory_id: "abc", new_content: "x" }, method: "PUT", pathPattern: /\/v1\/memories\/abc\/correct$/ },
      { id: "3.15", tool: "memory_correct", args: { query: "q", new_content: "x" }, method: "POST", pathPattern: /\/v1\/memories\/correct$/ },
      { id: "3.16", tool: "memory_purge", args: { topic: "old" }, method: "POST", pathPattern: /\/v1\/memories\/purge$/ },
      { id: "3.17", tool: "memory_observe", args: { messages: [] }, method: "POST", pathPattern: /\/v1\/observe$/ },
      { id: "3.18", tool: "memory_governance", args: {}, method: "POST", pathPattern: /\/v1\/governance$/ },
      { id: "3.19", tool: "memory_snapshot", args: { name: "s1" }, method: "POST", pathPattern: /\/v1\/snapshots$/ },
      { id: "3.20", tool: "memory_snapshots", args: {}, method: "GET", pathPattern: /\/v1\/snapshots\?/ },
      { id: "3.21", tool: "memory_rollback", args: { name: "s1" }, method: "POST", pathPattern: /\/v1\/snapshots\/s1\/rollback$/ },
      { id: "3.22", tool: "memory_branch", args: { name: "b1" }, method: "POST", pathPattern: /\/v1\/branches$/ },
      { id: "3.23", tool: "memory_branches", args: {}, method: "GET", pathPattern: /\/v1\/branches$/ },
      { id: "3.24", tool: "memory_checkout", args: { name: "b1" }, method: "POST", pathPattern: /\/v1\/branches\/b1\/checkout$/ },
      { id: "3.25", tool: "memory_branch_delete", args: { name: "b1" }, method: "DELETE", pathPattern: /\/v1\/branches\/b1$/ },
      { id: "3.26", tool: "memory_merge", args: { source: "b1" }, method: "POST", pathPattern: /\/v1\/branches\/b1\/merge$/ },
      { id: "3.27", tool: "memory_diff", args: { source: "b1" }, method: "GET", pathPattern: /\/v1\/branches\/b1\/diff\?/ },
    ];

    for (const { id, tool, args, method, pathPattern } of cases) {
      it(`${id} ${tool} → ${method} ${pathPattern.source}`, async () => {
        fetchMock.respondWith(200, tool === "memory_store"
          ? { memory_id: "new-id", content: args.content ?? "" }
          : tool.includes("retrieve") || tool.includes("search") || tool === "memory_list"
            ? { results: [] }
            : tool === "memory_profile"
              ? { profile: "test" }
              : tool === "memory_purge"
                ? { purged: 1 }
                : tool === "memory_correct"
                  ? { memory_id: "abc", content: "x" }
                  : tool === "memory_snapshots"
                    ? { snapshots: [] }
                    : tool === "memory_branches"
                      ? { branches: [] }
                      : { ok: true });
        await transport.callTool(tool, args);
        const call = fetchMock.lastCall()!;
        expect(call.method).toBe(method);
        expect(call.url).toMatch(pathPattern);
      });
    }

    it("3.28 memory_rebuild_index returns cloud message, no HTTP call", async () => {
      fetchMock.reset();
      const result = await transport.callTool("memory_rebuild_index", {});
      expect(fetchMock.calls.length).toBe(0);
      const text = JSON.parse((result as any).content[0].text);
      expect(text.message).toMatch(/cloud/i);
    });

    it("3.29 healthCheck → GET /health/instance", async () => {
      fetchMock.respondWith(200, { status: "ok", instance_id: "i-1", db: true });
      const result = await transport.healthCheck();
      expect(fetchMock.lastCall()!.method).toBe("GET");
      expect(fetchMock.lastCall()!.url).toMatch(/\/health\/instance$/);
      expect(result.status).toBe("ok");
      expect(result.instance_id).toBe("i-1");
    });

    it.each([
      ["an object without status", { instance_id: "i-1", db: true }],
      ["an empty status", { status: "   ", instance_id: "i-1", db: true }],
      ["a non-object response", "ok"],
    ])("3.29a healthCheck rejects %s", async (_description, response) => {
      fetchMock.respondWith(200, response);

      await expect(transport.healthCheck()).rejects.toThrow(/invalid health response/i);
    });

    it("3.30 unknown tool throws", async () => {
      await expect(transport.callTool("memory_nonexistent", {}))
        .rejects.toThrow(/Unknown Memoria tool/);
    });
  });

  // ── MCP envelope compatibility ───────────────────────────

  describe("MCP envelope", () => {
    it("3.31 callTool wraps result in MCP content block", async () => {
      fetchMock.respondWith(200, { memory_id: "x", content: "hello" });
      const result = await transport.callTool("memory_store", { content: "hello" }) as any;
      expect(result.content).toBeInstanceOf(Array);
      expect(result.content[0].type).toBe("text");
      expect(typeof result.content[0].text).toBe("string");
    });

    it("3.32 string result wrapped as-is", async () => {
      fetchMock.respondWith(200, { profile: "User likes cats" });
      const result = await transport.callTool("memory_profile", {}) as any;
      expect(result.content[0].text).toBe("User likes cats");
    });
  });

  // ── Response formatting ──────────────────────────────────

  describe("Response formatting", () => {
    it("3.34 memory_store → 'Stored memory <id>: <content>'", async () => {
      fetchMock.respondWith(200, { memory_id: "m1", content: "hello world" });
      const result = await transport.callTool("memory_store", { content: "hello world" }) as any;
      expect(result.content[0].text).toBe("Stored memory m1: hello world");
    });

    it.each([
      ["empty object", {}],
      ["null", null],
      ["array", []],
      ["numeric memory ID", { memory_id: 42 }],
      ["blank memory ID", { memory_id: "   " }],
      ["empty HTTP body", ""],
    ])("3.34a memory_store rejects %s", async (_case, response) => {
      fetchMock.respondWith(200, response);
      await expect(transport.callTool("memory_store", { content: "fixture" }))
        .rejects.toThrow(/invalid store response/i);
    });

    it("3.35 memory_retrieve preserves structured records", async () => {
      fetchMock.respondWith(200, {
        results: [
          { memory_id: "m1", memory_type: "semantic", content: "First" },
          { memory_id: "m2", memory_type: "profile", content: "Second" },
        ],
      });
      const result = await transport.callTool("memory_retrieve", { query: "q" }) as any;
      expect(JSON.parse(result.content[0].text)).toEqual([
        { memory_id: "m1", memory_type: "semantic", content: "First" },
        { memory_id: "m2", memory_type: "profile", content: "Second" },
      ]);
    });

    it("3.36 memory_retrieve empty → structured empty list", async () => {
      fetchMock.respondWith(200, { results: [] });
      const result = await transport.callTool("memory_retrieve", { query: "q" }) as any;
      expect(result.content[0].text).toBe("[]");
    });

    it.each(["memory_retrieve", "memory_search"])(
      "3.36a %s rejects malformed successful responses",
      async (tool) => {
        for (const response of [null, {}, { error: "upstream failed" }, { results: null }, "invalid", 42]) {
          fetchMock.respondWith(200, response);
          await expect(transport.callTool(tool, { query: "q" }))
            .rejects.toThrow(/invalid memoria api memory list response/i);
        }
      },
    );

    it.each(["memory_retrieve", "memory_search"])(
      "3.36b %s accepts every documented list envelope",
      async (tool) => {
        const item = { memory_id: "m1", content: "remember this" };
        for (const response of [[item], { results: [item] }, { items: [item] }]) {
          fetchMock.respondWith(200, response);
          const result = await transport.callTool(tool, { query: "q" }) as any;
          expect(JSON.parse(result.content[0].text)).toEqual([item]);
        }
      },
    );

    it("3.38 memory_profile with profile → profile text", async () => {
      fetchMock.respondWith(200, { profile: "Likes TypeScript" });
      const result = await transport.callTool("memory_profile", {}) as any;
      expect(result.content[0].text).toBe("Likes TypeScript");
    });

    it("3.39 memory_profile no profile → 'No profile memories found.'", async () => {
      fetchMock.respondWith(200, { profile: null });
      const result = await transport.callTool("memory_profile", {}) as any;
      expect(result.content[0].text).toBe("No profile memories found.");
    });

    it("3.40 memory_correct → 'Corrected memory <id>: <content>'", async () => {
      fetchMock.respondWith(200, { memory_id: "m1", content: "updated" });
      const result = await transport.callTool("memory_correct", {
        memory_id: "m1", new_content: "updated",
      }) as any;
      expect(result.content[0].text).toBe("Corrected memory m1: updated");
    });

    it.each([{ memory_id: "old-id" }, { query: "old memory" }])(
      "rejects invalid correction receipts without replay for %j",
      async (selector) => {
        for (const response of [null, {}, { error: "private upstream detail" },
          { memory_id: "" }, { memory_id: "  " }, { memory_id: 42 }, [], "invalid"]) {
          fetchMock.respondWith(200, response);
          const before = fetchMock.calls.length;
          await expect(transport.callTool("memory_correct", {
            ...selector, new_content: "replacement",
          })).rejects.toThrow("Memoria API returned invalid correction response: missing memory_id");
          expect(fetchMock.calls).toHaveLength(before + 1);
        }
      },
    );

    it.each([{ memory_id: "old-id" }, { query: "old memory" }])(
      "reports the authoritative replacement ID for %j",
      async (selector) => {
        fetchMock.respondWith(200, { memory_id: "replacement-id", content: "server content" });
        const result = await transport.callTool("memory_correct", {
          ...selector, new_content: "requested content",
        }) as any;
        expect(result.content[0].text).toBe("Corrected memory replacement-id: server content");
      },
    );

    it("3.41 memory_purge → 'Purged N memory(ies).'", async () => {
      fetchMock.respondWith(200, { purged: 3 });
      const result = await transport.callTool("memory_purge", { topic: "old" }) as any;
      expect(result.content[0].text).toBe("Purged 3 memory(ies).");
    });

    it("3.42 memory_snapshots with items → formatted list", async () => {
      fetchMock.respondWith(200, {
        snapshots: [
          { name: "snap1", timestamp: "2026-03-23T10:00:00Z" },
          { name: "snap2", timestamp: "2026-03-23T11:00:00Z" },
        ],
      });
      const result = await transport.callTool("memory_snapshots", {}) as any;
      const text = result.content[0].text;
      expect(text).toContain("Snapshots (2):");
      expect(text).toContain("snap1 (2026-03-23T10:00:00Z)");
      expect(text).toContain("snap2 (2026-03-23T11:00:00Z)");
    });

    it("3.43 memory_snapshots empty → 'Snapshots (0):'", async () => {
      fetchMock.respondWith(200, { snapshots: [] });
      const result = await transport.callTool("memory_snapshots", {}) as any;
      expect(result.content[0].text).toBe("Snapshots (0):");
    });

    it("3.44 memory_branches with items → formatted list with active marker", async () => {
      fetchMock.respondWith(200, {
        branches: [
          { name: "main", active: true },
          { name: "experiment", active: false },
        ],
      });
      const result = await transport.callTool("memory_branches", {}) as any;
      const text = result.content[0].text;
      expect(text).toContain("Branches:");
      expect(text).toContain("main ← active");
      expect(text).toContain("experiment");
      expect(text).not.toContain("experiment ← active");
    });

    it("3.45 memory_branches empty → default main active", async () => {
      fetchMock.respondWith(200, { branches: [] });
      const result = await transport.callTool("memory_branches", {}) as any;
      expect(result.content[0].text).toContain("main ← active");
    });
  });

  // ── Error handling ───────────────────────────────────────

  describe("Error handling", () => {
    it("3.46 non-2xx HTTP → error with method, path, status", async () => {
      fetchMock.respondWith(500, "Internal Server Error");
      await expect(transport.callTool("memory_retrieve", { query: "q" }))
        .rejects.toThrow(/POST.*\/v1\/memories\/retrieve.*500/);
    });

    it("3.48 empty response body → { ok: true }", async () => {
      // Override fetch to return empty body
      globalThis.fetch = async () => new Response("", { status: 200 });
      const t = new MemoriaHttpTransport(buildApiConfig(), "u");
      const result = await t.callTool("memory_consolidate", {}) as any;
      // Should not throw, result wraps { ok: true }
      expect(result.content[0].text).toContain("ok");
    });
  });
});
