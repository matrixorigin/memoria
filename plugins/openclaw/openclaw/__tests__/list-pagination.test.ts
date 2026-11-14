/**
 * Cursor pagination for GET /v1/memories (issue #267).
 * The API caps each page at 500 rows and returns next_cursor when more exist.
 */
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { MemoriaClient } from "../client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;
const servers: Server[] = [];

afterEach(async () => {
  globalThis.fetch = originalFetch;
  for (const server of servers.splice(0)) {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

const SERVER_PAGE_MAX = 500;

function memoryId(i: number) {
  return i.toString(16).padStart(32, "0");
}

/**
 * Emulates the REST routes the client touches: list_memories (keyset pagination
 * over `total` rows, page size clamped to 500), get_memory, and empty
 * snapshot/branch listings. Any other path fails the test.
 */
function serveMemories(f: ReturnType<typeof mockFetch>, total: number) {
  f.respondWithHandler((url) => {
    const parsed = new URL(url);
    const single = parsed.pathname.match(/^\/v1\/memories\/([^/]+)$/);
    if (single) {
      const index = Number.parseInt(single[1], 16);
      const found = memoryId(index) === single[1] && index < total;
      return {
        status: 200,
        body: found ? { memory_id: single[1], content: `m${index}`, memory_type: "semantic" } : null,
      };
    }
    if (parsed.pathname === "/v1/snapshots") {
      return { status: 200, body: { snapshots: [], total: 0, limit: 0, offset: 0 } };
    }
    if (parsed.pathname === "/v1/branches") {
      return {
        status: 200,
        body: { branches: [{ name: "main", active: true }], result: "Branches:\nmain ← active" },
      };
    }
    if (parsed.pathname !== "/v1/memories") {
      throw new Error(`unexpected request: ${parsed.pathname}`);
    }
    const limit = Math.min(Number(parsed.searchParams.get("limit") ?? 100), SERVER_PAGE_MAX);
    const cursor = parsed.searchParams.get("cursor");
    const start = cursor ? Number.parseInt(cursor, 16) + 1 : 0;
    const end = Math.min(start + limit, total);
    const items = [];
    for (let i = start; i < end; i++) {
      items.push({ memory_id: memoryId(i), content: `m${i}`, memory_type: "semantic" });
    }
    return {
      status: 200,
      body: { items, next_cursor: end < total ? memoryId(end - 1) : null },
    };
  });
}

describe("listMemories cursor pagination", () => {
  it.each([
    ["null", null],
    ["empty object", {}],
    ["error envelope", { error: "upstream failed" }],
    ["null items", { items: null }],
    ["unexpected text", "invalid"],
    ["unexpected number", 42],
    ["non-string cursor", { items: [], next_cursor: 42 }],
    ["empty response", ""],
  ])("rejects malformed successful list pages: %s", async (_case, response) => {
    const f = mockFetch();
    f.respondWith(200, response);
    const client = new MemoriaClient(buildApiConfig());
    try {
      await expect(client.listMemories({ userId: "u", limit: 10 }))
        .rejects.toThrow(/invalid memoria api list page/i);
    } finally {
      client.close();
    }
  });

  it("rejects malformed later pages instead of returning an incomplete list as complete", async () => {
    const f = mockFetch();
    const id = memoryId(0);
    f.respondWithHandler((url) => ({
      status: 200,
      body: new URL(url).searchParams.has("cursor")
        ? { error: "unexpected page" }
        : { items: [{ memory_id: id, content: "first", memory_type: "semantic" }], next_cursor: id },
    }));
    const client = new MemoriaClient(buildApiConfig());
    try {
      await expect(client.listMemories({ userId: "u", limit: 10 }))
        .rejects.toThrow(/invalid memoria api list page/i);
      expect(f.calls).toHaveLength(2);
    } finally {
      client.close();
    }
  });

  it("preserves legacy array-only list responses", async () => {
    const f = mockFetch();
    f.respondWith(200, [{ memory_id: memoryId(0), content: "first", memory_type: "semantic" }]);
    const client = new MemoriaClient(buildApiConfig());
    try {
      const result = await client.listMemories({ userId: "u", limit: 10 });
      expect(result.items.map((item) => item.memory_id)).toEqual([memoryId(0)]);
      expect(result.partial).toBe(false);
    } finally {
      client.close();
    }
  });

  it.each([
    { limit: 1, pages: 1, ids: [2], partial: true },
    { limit: 3, pages: 2, ids: [2, 3, 4], partial: false },
  ])("filters by type before applying the API page budget: $limit rows", async ({ limit, pages, ids, partial }) => {
    const requests: URL[] = [];
    const rows = Array.from({ length: 5 }, (_, i) => ({
      memory_id: memoryId(i), content: `m${i}`,
      memory_type: i < 2 ? "semantic" : "episodic",
    }));
    const server = createServer((request, response) => {
      const url = new URL(request.url!, "http://localhost");
      requests.push(url);
      const type = url.searchParams.get("memory_type");
      const selected = type ? rows.filter((row) => row.memory_type === type) : rows;
      const cursor = url.searchParams.get("cursor");
      const start = cursor ? selected.findIndex((row) => row.memory_id === cursor) + 1 : 0;
      const size = Math.min(2, Number(url.searchParams.get("limit")));
      const items = selected.slice(start, start + size);
      const more = start + items.length < selected.length;
      response.setHeader("Content-Type", "application/json");
      response.end(JSON.stringify({ items, next_cursor: more ? items.at(-1)!.memory_id : null }));
    });
    servers.push(server);
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const client = new MemoriaClient(buildApiConfig({
      apiUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
      maxListPages: pages,
    }));
    try {
      const result = await client.listMemories({ userId: "u", limit, memoryType: "episodic" });
      expect(result.items.map((row) => row.memory_id)).toEqual(ids.map(memoryId));
      expect(result.count).toBe(ids.length);
      expect(result.partial).toBe(partial);
      expect(result.limitations).toBeUndefined();
      expect(requests).toHaveLength(pages);
      expect(requests.every((url) => url.searchParams.get("memory_type") === "episodic")).toBe(true);
      expect(requests.map((url) => Number(url.searchParams.get("limit")))).toEqual(
        pages === 1 ? [1] : [3, 1],
      );
    } finally {
      client.close();
    }
  });

  it("issue #267 repro: a truncated page with next_cursor is never reported complete", async () => {
    const f = mockFetch();
    const client = new MemoriaClient(buildApiConfig({ maxListPages: 1 }));
    f.respondWith(200, {
      items: Array.from({ length: 500 }, (_, i) => ({
        memory_id: memoryId(i), content: "test", memory_type: "semantic", trust_tier: "T3",
      })),
      next_cursor: "a".repeat(32),
    });
    const result = await client.listMemories({ userId: "u", limit: 1000 });
    expect(result.items.length === 1000 || result.partial).toBe(true);
    client.close();
  });

  it("follows next_cursor past the 500-row server cap", async () => {
    const f = mockFetch();
    serveMemories(f, 1200);
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.listMemories({ userId: "u", limit: 1000 });
    expect(result.count).toBe(1000);
    expect(new Set(result.items.map((m) => m.memory_id)).size).toBe(1000);
    expect(result.partial).toBe(true);
    expect(f.calls.map((c) => new URL(c.url).search)).toEqual([
      "?limit=500",
      `?limit=500&cursor=${memoryId(499)}`,
    ]);
    client.close();
  });

  it("reports complete when every row fits within the limit", async () => {
    const f = mockFetch();
    serveMemories(f, 700);
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.listMemories({ userId: "u", limit: 1000 });
    expect(result.count).toBe(700);
    expect(result.partial).toBe(false);
    expect(f.calls).toHaveLength(2);
    client.close();
  });

  it("reports partial when exactly the limit is returned and more rows exist", async () => {
    const f = mockFetch();
    serveMemories(f, 501);
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.listMemories({ userId: "u", limit: 500 });
    expect(result.count).toBe(500);
    expect(result.partial).toBe(true);
    expect(f.calls).toHaveLength(1);
    client.close();
  });

  it("reports complete when the last page lands exactly on a page boundary", async () => {
    const f = mockFetch();
    serveMemories(f, 500);
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.listMemories({ userId: "u", limit: 1000 });
    expect(result.count).toBe(500);
    expect(result.partial).toBe(false);
    client.close();
  });

  it("stops at maxListPages and marks the result partial", async () => {
    const f = mockFetch();
    serveMemories(f, 2000);
    const client = new MemoriaClient(buildApiConfig({ maxListPages: 2 }));
    const result = await client.listMemories({ userId: "u", limit: 2000 });
    expect(f.calls).toHaveLength(2);
    expect(result.count).toBe(1000);
    expect(result.partial).toBe(true);
    client.close();
  });

  it("stops on a cursor that does not advance", async () => {
    const f = mockFetch();
    f.respondWith(200, {
      items: [{ memory_id: memoryId(0), content: "x", memory_type: "semantic" }],
      next_cursor: memoryId(0),
    });
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.listMemories({ userId: "u", limit: 1000 });
    expect(f.calls).toHaveLength(2);
    // the repeated page is dropped, so its row is not counted twice
    expect(result.items.map((m) => m.memory_id)).toEqual([memoryId(0)]);
    expect(result.count).toBe(1);
    expect(result.partial).toBe(true);
    client.close();
  });

  it.each([2, 3])("stops a %i-page cursor cycle without counting repeated rows", async (cycleLength) => {
    const f = mockFetch();
    f.respondWithHandler((url) => {
      const cursor = new URL(url).searchParams.get("cursor");
      const index = cursor === null ? 0 : (Number.parseInt(cursor, 16) + 1) % cycleLength;
      return { status: 200, body: {
        items: [{ memory_id: memoryId(index), content: `m${index}`, memory_type: "semantic" }],
        next_cursor: memoryId(index),
      } };
    });
    const client = new MemoriaClient(buildApiConfig());
    try {
      const result = await client.listMemories({ userId: "u", limit: 1000 });
      expect(f.calls).toHaveLength(cycleLength + 1);
      expect(result.items.map((m) => m.memory_id))
        .toEqual(Array.from({ length: cycleLength }, (_, i) => memoryId(i)));
      expect(result.count).toBe(cycleLength);
      expect(result.partial).toBe(true);
    } finally {
      client.close();
    }
  });

  it("stats counts memories beyond the first page", async () => {
    const f = mockFetch();
    serveMemories(f, 800);
    const client = new MemoriaClient(buildApiConfig());
    // stats also lists snapshots and branches; the handler answers with none and main only
    const stats = await client.stats("u");
    expect(stats.activeMemoryCount).toBe(800);
    expect(stats.snapshotCount).toBe(0);
    expect(stats.branchCount).toBe(1);
    client.close();
  });
});

describe("getMemory", () => {
  it("finds an uncached memory that lives on a later page", async () => {
    const f = mockFetch();
    serveMemories(f, 1500);
    const client = new MemoriaClient(buildApiConfig());
    const memory = await client.getMemory({ userId: "u", memoryId: memoryId(1400) });
    expect(memory?.memory_id).toBe(memoryId(1400));
    expect(f.calls).toHaveLength(1);
    expect(new URL(f.calls[0].url).pathname).toBe(`/v1/memories/${memoryId(1400)}`);
    client.close();
  });

  it("returns null only when the API says the memory does not exist", async () => {
    const f = mockFetch();
    serveMemories(f, 10);
    const client = new MemoriaClient(buildApiConfig());
    expect(await client.getMemory({ userId: "u", memoryId: memoryId(99) })).toBeNull();
    client.close();
  });

  it("propagates API errors instead of reporting the memory as missing", async () => {
    const f = mockFetch();
    f.respondWith(500, { error: "boom" });
    const client = new MemoriaClient(buildApiConfig());
    await expect(client.getMemory({ userId: "u", memoryId: memoryId(1) })).rejects.toThrow(/500/);
    client.close();
  });

  it("ignores a payload that is not the requested memory", async () => {
    const f = mockFetch();
    f.respondWith(200, "");
    const client = new MemoriaClient(buildApiConfig());
    expect(await client.getMemory({ userId: "u", memoryId: memoryId(1) })).toBeNull();
    // nothing bogus was cached: the next lookup goes back to the API
    expect(await client.getMemory({ userId: "u", memoryId: memoryId(1) })).toBeNull();
    expect(f.calls).toHaveLength(2);
    client.close();
  });

  it("serves later lookups from the cache", async () => {
    const f = mockFetch();
    serveMemories(f, 10);
    const client = new MemoriaClient(buildApiConfig());
    await client.getMemory({ userId: "u", memoryId: memoryId(3) });
    await client.getMemory({ userId: "u", memoryId: memoryId(3) });
    expect(f.calls).toHaveLength(1);
    client.close();
  });
});
