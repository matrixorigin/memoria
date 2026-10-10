/**
 * Cache invalidation: getMemory must not serve records that a later operation
 * deleted, superseded, or made invisible.
 *
 * The cache key is `${userId}::${memoryId}` — it carries no branch or version —
 * so operations that change which memories are visible have to drop the entries
 * they can no longer vouch for, while leaving other users' entries alone.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { MemoriaClient } from "../client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const MEMORY = {
  memory_id: "m1",
  content: "old content",
  memory_type: "semantic",
  trust_tier: "T3",
  is_active: true,
};

let originalFetch: typeof globalThis.fetch;

beforeEach(() => {
  originalFetch = globalThis.fetch;
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

/** Cache m1 for `userId` by retrieving it, then hand back the mock. */
async function cacheM1(client: MemoriaClient, f: ReturnType<typeof mockFetch>, userId = "u") {
  f.respondWith(200, [MEMORY]);
  await client.retrieve({ userId, query: "old", topK: 5 });
  // Confirm it really is cached: no further fetch should be needed.
  const before = f.calls.length;
  expect(await client.getMemory({ userId, memoryId: "m1" })).not.toBeNull();
  expect(f.calls.length).toBe(before);
}

/** After invalidation getMemory must consult the backend, which reports nothing. */
async function expectRevalidatedToNull(
  client: MemoriaClient,
  f: ReturnType<typeof mockFetch>,
  userId = "u",
) {
  f.respondWith(200, { items: [], next_cursor: null });
  const before = f.calls.length;
  expect(await client.getMemory({ userId, memoryId: "m1" })).toBeNull();
  expect(f.calls.length).toBeGreaterThan(before);
}

describe("memory cache invalidation", () => {
  it("topic purge invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "u", topic: "old" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("correctById stops serving the superseded record", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { memory_id: "m2", content: "new content" });
      await c.correctById({ userId: "u", memoryId: "m1", newContent: "new content" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("correctByQuery invalidates the user's cache", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { memory_id: "m2", content: "new content" });
      await c.correctByQuery({ userId: "u", query: "old", newContent: "new content" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("branch checkout invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Switched to branch experiment" });
      await c.branchCheckout({ userId: "u", name: "experiment" });
      // Same id, different content on the other branch.
      f.respondWith(200, { ...MEMORY, content: "experiment content" });
      const fetched = await c.getMemory({ userId: "u", memoryId: "m1" });
      expect(fetched?.content).toBe("experiment content");
    } finally {
      c.close();
    }
  });

  it("branch merge invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Merged" });
      await c.branchMerge({ userId: "u", source: "experiment", strategy: "accept" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("snapshot rollback invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Rolled back" });
      await c.rollbackSnapshot({ userId: "u", name: "before" });
      f.respondWith(200, { ...MEMORY, content: "restored content" });
      const fetched = await c.getMemory({ userId: "u", memoryId: "m1" });
      expect(fetched?.content).toBe("restored content");
    } finally {
      c.close();
    }
  });

  // Control from the issue: this path already worked and must keep working.
  it("deleteMemory still drops its own entry", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { purged: 1 });
      await c.deleteMemory({ userId: "u", memoryId: "m1" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("purge by memory_id leaves other cached ids alone", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      f.respondWith(200, [MEMORY, { ...MEMORY, memory_id: "m9", content: "keep me" }]);
      await c.retrieve({ userId: "u", query: "old", topK: 5 });
      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "u", memoryId: "m1" });

      const before = f.calls.length;
      const kept = await c.getMemory({ userId: "u", memoryId: "m9" });
      expect(kept?.content).toBe("keep me");
      expect(f.calls.length).toBe(before);
    } finally {
      c.close();
    }
  });

  it("invalidation does not leak across users", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f, "alice");
      await cacheM1(c, f, "bob");

      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "alice", topic: "old" });

      // bob's entry is untouched: still served from cache.
      const before = f.calls.length;
      expect(await c.getMemory({ userId: "bob", memoryId: "m1" })).not.toBeNull();
      expect(f.calls.length).toBe(before);

      // alice's is gone.
      await expectRevalidatedToNull(c, f, "alice");
    } finally {
      c.close();
    }
  });

  it("branch delete invalidates cached memories (implicit switch to main)", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      // Deleting the active branch resets the backend to main.
      f.respondWith(200, { result: "Deleted branch experiment" });
      await c.branchDelete({ userId: "u", name: "experiment" });
      f.respondWith(200, { ...MEMORY, content: "main branch content" });
      const fetched = await c.getMemory({ userId: "u", memoryId: "m1" });
      expect(fetched?.content).toBe("main branch content");
    } finally {
      c.close();
    }
  });

  it("an in-flight read does not repopulate the cache after invalidation", async () => {
    // Order: start a retrieval, hold its response, purge, then release the
    // pre-purge response. It describes a state the backend has already left.
    const originalFetch = globalThis.fetch;
    let releaseRetrieve: (() => void) | undefined;
    const retrieveReached = new Promise<void>((resolveReached) => {
      globalThis.fetch = (async (url: string | URL | Request, init?: RequestInit) => {
        const urlStr = typeof url === "string" ? url : url instanceof URL ? url.toString() : url.url;
        if (urlStr.includes("retrieve")) {
          resolveReached();
          await new Promise<void>((r) => {
            releaseRetrieve = r;
          });
          return new Response(JSON.stringify([MEMORY]), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response(JSON.stringify({ purged: 1 }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }) as typeof globalThis.fetch;
    });

    const c = new MemoriaClient(buildApiConfig());
    try {
      const inFlight = c.retrieve({ userId: "u", query: "old", topK: 5 });
      await retrieveReached;

      await c.purgeMemory({ userId: "u", topic: "old" });

      releaseRetrieve?.();
      await inFlight;

      // The stale response must not have been written back into the cache.
      globalThis.fetch = (async () =>
        new Response(JSON.stringify({ items: [], next_cursor: null }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        })) as typeof globalThis.fetch;
      expect(await c.getMemory({ userId: "u", memoryId: "m1" })).toBeNull();
    } finally {
      c.close();
      globalThis.fetch = originalFetch;
    }
  });

  it("a failed purge keeps the cache", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(500, { error: "boom" });
      await expect(c.purgeMemory({ userId: "u", topic: "old" })).rejects.toThrow();

      const before = f.calls.length;
      expect(await c.getMemory({ userId: "u", memoryId: "m1" })).not.toBeNull();
      expect(f.calls.length).toBe(before);
    } finally {
      c.close();
    }
  });
});

// ── Generation must also advance for id-scoped invalidation, and corrections
//    must respect a generation captured before their own request ──────────────

/**
 * Install a fetch that defers the first request matching `deferOn` until the
 * returned `release` is called, answering everything else immediately from
 * `respond`. Lets a read be held open across a mutation.
 */
function deferredFetch(
  deferOn: (url: string) => boolean,
  deferredBody: unknown,
  respond: (url: string) => unknown,
) {
  let release: (() => void) | undefined;
  let reached: () => void;
  const reachedPromise = new Promise<void>((r) => {
    reached = r;
  });
  let deferredAlready = false;

  globalThis.fetch = (async (url: string | URL | Request) => {
    const urlStr = typeof url === "string" ? url : url instanceof URL ? url.toString() : url.url;
    const json = (body: unknown) =>
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    if (!deferredAlready && deferOn(urlStr)) {
      deferredAlready = true;
      reached();
      await new Promise<void>((r) => {
        release = r;
      });
      return json(deferredBody);
    }
    return json(respond(urlStr));
  }) as typeof globalThis.fetch;

  return { reached: reachedPromise, release: () => release?.() };
}

describe("cache generation across concurrent operations", () => {
  // An id-scoped mutation must advance the generation, or a retrieval dispatched
  // before it can restore exactly the record that was just removed.
  const idScopedCases: Array<[string, (c: MemoriaClient) => Promise<unknown>]> = [
    ["correctById", (c) => c.correctById({ userId: "u", memoryId: "m1", newContent: "new" })],
    ["deleteMemory", (c) => c.deleteMemory({ userId: "u", memoryId: "m1" })],
    ["purge by memoryId", (c) => c.purgeMemory({ userId: "u", memoryId: "m1" })],
  ];

  for (const [name, mutate] of idScopedCases) {
    it(`${name} stops an in-flight read from restoring the record`, async () => {
      const f = deferredFetch(
        (url) => url.includes("retrieve"),
        [MEMORY],
        () => ({ memory_id: "m2", content: "new", purged: 1 }),
      );
      const c = new MemoriaClient(buildApiConfig());
      try {
        const inFlight = c.retrieve({ userId: "u", query: "old", topK: 5 });
        await f.reached;

        await mutate(c);

        f.release();
        await inFlight;

        globalThis.fetch = (async () =>
          new Response(JSON.stringify({ items: [], next_cursor: null }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          })) as typeof globalThis.fetch;
        expect(await c.getMemory({ userId: "u", memoryId: "m1" })).toBeNull();
      } finally {
        c.close();
      }
    });
  }

  // A correction whose response lands after a checkout describes the branch the
  // backend has already left, so its replacement must not be cached.
  const correctionCases: Array<[string, (c: MemoriaClient) => Promise<unknown>]> = [
    ["correctById", (c) => c.correctById({ userId: "u", memoryId: "m1", newContent: "new" })],
    ["correctByQuery", (c) => c.correctByQuery({ userId: "u", query: "old", newContent: "new" })],
  ];

  for (const [name, correct] of correctionCases) {
    it(`${name} does not cache a replacement that lands after a checkout`, async () => {
      const f = deferredFetch(
        (url) => url.includes("correct"),
        { memory_id: "m2", content: "old-branch replacement" },
        () => ({ result: "Switched to branch main" }),
      );
      const c = new MemoriaClient(buildApiConfig());
      try {
        const inFlight = correct(c);
        await f.reached;

        await c.branchCheckout({ userId: "u", name: "main" });

        f.release();
        await inFlight;

        // main has no such record; the cache must not answer from the old branch.
        globalThis.fetch = (async () =>
          new Response(JSON.stringify({ items: [], next_cursor: null }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          })) as typeof globalThis.fetch;
        expect(await c.getMemory({ userId: "u", memoryId: "m2" })).toBeNull();
      } finally {
        c.close();
      }
    });
  }
});

// ── An unrelated mutation must not suppress a correction's own invalidation ──
// invalidateMemoryIds keeps unrelated ids, so a changed generation does not mean
// the cache is empty: the correction still has to drop what it superseded.

describe("correction invalidates regardless of unrelated generation changes", () => {
  const cases: Array<[string, (c: MemoriaClient) => Promise<unknown>]> = [
    ["correctById", (c) => c.correctById({ userId: "u", memoryId: "m1", newContent: "new" })],
    ["correctByQuery", (c) => c.correctByQuery({ userId: "u", query: "old", newContent: "new" })],
  ];

  for (const [name, correct] of cases) {
    it(`${name} still drops the superseded record after an unrelated delete`, async () => {
      const f = mockFetch();
      const c = new MemoriaClient(buildApiConfig());
      try {
        // 1. cache m1 (old active content) and unrelated m9
        f.respondWith(200, [MEMORY, { ...MEMORY, memory_id: "m9", content: "unrelated" }]);
        await c.retrieve({ userId: "u", query: "old", topK: 5 });

        // 2. start the correction of m1 and hold its replacement response
        const deferred = deferredFetch(
          (url) => url.includes("correct"),
          { memory_id: "m2", content: "new content" },
          () => ({ purged: 1 }),
        );
        const inFlight = correct(c);
        await deferred.reached;

        // 3. delete unrelated m9 — advances the generation, leaves m1 cached
        await c.deleteMemory({ userId: "u", memoryId: "m9" });

        // 4. release the correction
        deferred.release();
        await inFlight;

        // 5. m1 was superseded; it must not still answer from cache
        globalThis.fetch = (async () =>
          new Response(JSON.stringify({ items: [], next_cursor: null }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          })) as typeof globalThis.fetch;
        expect(await c.getMemory({ userId: "u", memoryId: "m1" })).toBeNull();
      } finally {
        c.close();
      }
    });
  }
});
