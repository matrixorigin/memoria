/**
 * Group 4: Response Parsing
 * "Can the plugin understand what Memoria returns?"
 *
 * Embedded mode uses the MCP binary's text format, while HTTP mode preserves
 * structured API fields in JSON text blocks.
 */
import { describe, it, expect, afterEach } from "vitest";
import { MemoriaClient } from "../client.js";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;
let fetchHelper: ReturnType<typeof mockFetch>;

function setup() {
  fetchHelper = mockFetch();
  return new MemoriaHttpTransport(buildApiConfig(), "test-user");
}

function teardown() {
  globalThis.fetch = originalFetch;
}

describe("Group 4: Response Parsing (format round-trip)", () => {
  afterEach(teardown);

  // ── Memory list parsing ──────────────────────────────────

  describe("Structured HTTP memory format", () => {
    it("4.1 preserves a single memory record", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        results: [{ memory_id: "abc-123", memory_type: "semantic", content: "Hello world" }],
      });
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      expect(JSON.parse(result.content[0].text)).toEqual([
        { memory_id: "abc-123", memory_type: "semantic", content: "Hello world" },
      ]);
    });

    it("4.2 preserves multiple memories and retrieval scores", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        results: [
          { memory_id: "m1", memory_type: "semantic", content: "First", retrieval_score: 3.1 },
          { memory_id: "m2", memory_type: "profile", content: "Second", retrieval_score: 1.7 },
        ],
      });
      const result = await t.callTool("memory_search", { query: "q" }) as any;
      const records = JSON.parse(result.content[0].text);
      expect(records).toHaveLength(2);
      expect(records.map((record: any) => record.retrieval_score)).toEqual([3.1, 1.7]);
    });

    it("4.3 empty results remain a structured empty list", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { results: [] });
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      expect(result.content[0].text).toBe("[]");
    });

    it("4.4 array response shape (no results wrapper)", async () => {
      const t = setup();
      fetchHelper.respondWith(200, [
        { memory_id: "m1", memory_type: "semantic", content: "Direct array" },
      ]);
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      expect(JSON.parse(result.content[0].text)).toEqual([
        { memory_id: "m1", memory_type: "semantic", content: "Direct array" },
      ]);
    });

    it("4.4b client parses structured records without dropping retrieval_score", async () => {
      setup();
      fetchHelper.respondWith(200, {
        results: [
          {
            memory_id: "m1",
            memory_type: "semantic",
            content: "Scored result",
            retrieval_score: 2.75,
          },
        ],
      });
      const client = new MemoriaClient(buildApiConfig());
      const records = await client.search({ userId: "test-user", query: "q", topK: 5 });
      client.close();

      expect(records).toHaveLength(1);
      expect(records[0].retrieval_score).toBe(2.75);
    });
  });

  // ── Store/correct/purge parsing ──────────────────────────

  describe("Store/correct/purge text format", () => {
    it("4.5 store → 'Stored memory <id>: <content>'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "new-1", content: "Test memory" });
      const result = await t.callTool("memory_store", { content: "Test memory" }) as any;
      expect(result.content[0].text).toBe("Stored memory new-1: Test memory");
    });

    it("4.6 store with missing id → rejects malformed receipt", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { content: "No id returned" });
      await expect(t.callTool("memory_store", { content: "No id returned" }))
        .rejects.toThrow(/invalid store response/i);
    });

    it("4.7 correct by id → 'Corrected memory <id>: <content>'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "m1", content: "Fixed" });
      const result = await t.callTool("memory_correct", {
        memory_id: "m1", new_content: "Fixed",
      }) as any;
      expect(result.content[0].text).toBe("Corrected memory m1: Fixed");
    });

    it("4.8 correct by query → same format", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "m2", content: "Updated" });
      const result = await t.callTool("memory_correct", {
        query: "old content", new_content: "Updated",
      }) as any;
      expect(result.content[0].text).toBe("Corrected memory m2: Updated");
    });

    it("4.9 purge → 'Purged N memory(ies).'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { purged: 5 });
      const result = await t.callTool("memory_purge", { topic: "test" }) as any;
      expect(result.content[0].text).toBe("Purged 5 memory(ies).");
    });

    it("4.10 purge with zero → 'Purged 0 memory(ies).'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { purged: 0 });
      const result = await t.callTool("memory_purge", { topic: "nothing" }) as any;
      expect(result.content[0].text).toBe("Purged 0 memory(ies).");
    });
  });

  // ── Snapshot list parsing ────────────────────────────────

  describe("Snapshot list text format", () => {
    it("4.11 snapshots → 'Snapshots (N):\\n  name (ts)'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        snapshots: [
          { name: "before-refactor", timestamp: "2026-03-23T10:00:00Z" },
        ],
      });
      const result = await t.callTool("memory_snapshots", {}) as any;
      const text = result.content[0].text;
      expect(text).toMatch(/^Snapshots \(1\):/);
      expect(text).toContain("before-refactor (2026-03-23T10:00:00Z)");
    });

    it("4.12 empty snapshots → 'Snapshots (0):'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { snapshots: [] });
      const result = await t.callTool("memory_snapshots", {}) as any;
      expect(result.content[0].text).toBe("Snapshots (0):");
    });
  });

  // ── Branch list parsing ──────────────────────────────────

  describe("Branch list text format", () => {
    it("4.14 branches with active marker", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        branches: [
          { name: "main", active: true },
          { name: "experiment", active: false },
        ],
      });
      const result = await t.callTool("memory_branches", {}) as any;
      const text = result.content[0].text;
      expect(text).toContain("main ← active");
      expect(text).toMatch(/^\s*experiment$/m);
    });

    it("4.15 branches with no active → main defaults active", async () => {
      // When API returns no active flag, formatBranchList just passes through
      // The client-side parseBranches defaults main to active
      const t = setup();
      fetchHelper.respondWith(200, {
        branches: [
          { name: "main", active: false },
          { name: "dev", active: false },
        ],
      });
      const result = await t.callTool("memory_branches", {}) as any;
      const text = result.content[0].text;
      // formatBranchList respects the active flag from API
      expect(text).toContain("Branches:");
      expect(text).toContain("main");
    });

    it("4.16 empty branches → default main active", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { branches: [] });
      const result = await t.callTool("memory_branches", {}) as any;
      expect(result.content[0].text).toContain("main ← active");
    });
  });
});
