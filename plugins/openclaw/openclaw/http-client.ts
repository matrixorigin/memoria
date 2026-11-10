/**
 * MemoriaHttpTransport — direct HTTP client for the Memoria REST API.
 *
 * Replaces the MCP stdio bridge (MemoriaMcpSession) when backend === "api".
 * No Rust binary needed. Uses native fetch().
 */

import type { MemoriaPluginConfig } from "./config.js";

/** Server-side cap on GET /v1/memories page size (see list_memories in memoria-api). */
const API_LIST_PAGE_MAX = 500;

export class MemoriaHttpTransport {
  private readonly apiUrl: string;
  private readonly apiKey: string;
  private readonly timeoutMs: number;
  private readonly maxListPages: number;

  constructor(config: MemoriaPluginConfig, private readonly userId: string) {
    if (!config.apiUrl) {
      throw new Error("apiUrl is required for api backend mode");
    }
    if (!config.apiKey) {
      throw new Error("apiKey is required for api backend mode");
    }
    this.apiUrl = config.apiUrl.replace(/\/+$/, "");
    this.apiKey = config.apiKey;
    this.timeoutMs = config.timeoutMs;
    this.maxListPages = config.maxListPages;
  }

  /** Matches MemoriaMcpSession.isAlive() — HTTP transport is always "alive". */
  isAlive(): boolean {
    return true;
  }

  /** No-op for HTTP transport (no child process to kill). */
  close(): void {}

  /**
   * Unified entry point that mirrors MemoriaMcpSession.callTool().
   * Maps MCP tool names to REST API calls and returns the same
   * text-content structure the MCP session would return.
   */
  async callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
    const result = await this.dispatch(name, args);
    if (name === "memory_branches") {
      // Keep the human-readable envelope, but do not make callers recover
      // branch identities or active flags from ambiguous display text.
      return {
        content: [{ type: "text", text: this.formatBranchList(result) }],
        structuredContent: Array.isArray(result) ? { branches: result } : result,
      };
    }
    // Wrap in MCP-compatible content block so extractToolText() works unchanged
    return { content: [{ type: "text", text: typeof result === "string" ? result : JSON.stringify(result) }] };
  }

  // ── REST dispatch ──────────────────────────────────────────────

  private async dispatch(tool: string, args: Record<string, unknown>): Promise<unknown> {
    switch (tool) {
      case "memory_store":
        return this.store(args);
      case "memory_retrieve":
        return this.retrieve(args);
      case "memory_search":
        return this.search(args);
      case "memory_list":
        return this.list(args);
      case "memory_get":
        return this.getOne(args);
      case "memory_profile":
        return this.profile();
      case "memory_correct":
        return this.correct(args);
      case "memory_purge":
        return this.purge(args);
      case "memory_observe":
        return this.observe(args);
      case "memory_governance":
        return this.governance(args);
      case "memory_consolidate":
        return this.consolidate(args);
      case "memory_reflect":
        return this.reflect(args);
      case "memory_extract_entities":
        return this.extractEntities(args);
      case "memory_link_entities":
        return this.linkEntities(args);
      case "memory_rebuild_index":
        return { message: "rebuild_index is managed by the cloud service and not available via API." };
      case "memory_snapshot":
        return this.createSnapshot(args);
      case "memory_snapshots":
        return this.listSnapshots(args);
      case "memory_rollback":
        return this.rollbackSnapshot(args);
      case "memory_branch":
        return this.branchCreate(args);
      case "memory_branches":
        return this.branchList();
      case "memory_checkout":
        return this.branchCheckout(args);
      case "memory_branch_delete":
        return this.branchDelete(args);
      case "memory_merge":
        return this.branchMerge(args);
      case "memory_diff":
        return this.branchDiff(args);
      default:
        throw new Error(`Unknown Memoria tool: ${tool}`);
    }
  }

  // ── Memory CRUD ────────────────────────────────────────────────

  private async store(args: Record<string, unknown>) {
    const body: Record<string, unknown> = { content: args.content };
    if (args.memory_type) body.memory_type = args.memory_type;
    if (args.session_id) body.session_id = args.session_id;
    if (args.trust_tier) body.trust_tier = args.trust_tier;
    const data = await this.post("/v1/memories", body);
    const rec = this.asRecord(data);
    const id = rec?.memory_id;
    if (typeof id !== "string" || !id.trim()) {
      throw new Error("Memoria API returned invalid store response: missing memory_id");
    }
    const content = rec?.content ?? args.content ?? "";
    return `Stored memory ${id}: ${content}`;
  }

  private async retrieve(args: Record<string, unknown>) {
    const body: Record<string, unknown> = {
      query: args.query,
      top_k: args.top_k ?? 5,
    };
    if (args.session_id) body.session_id = args.session_id;
    if (args.memory_types !== undefined) body.memory_types = args.memory_types;
    const data = await this.post("/v1/memories/retrieve", body);
    return this.formatMemoryList(data);
  }

  private async search(args: Record<string, unknown>) {
    const body: Record<string, unknown> = {
      query: args.query,
      top_k: args.top_k ?? 10,
    };
    const data = await this.post("/v1/memories/search", body);
    return this.formatMemoryList(data);
  }

  /**
   * Follows next_cursor until `limit` items are collected, the server reports no
   * more rows, or maxListPages is reached. Returns `{ items, has_more }` so the
   * client can tell a truncated scan from a complete one.
   */
  private async list(args: Record<string, unknown>) {
    const limit = typeof args.limit === "number" ? args.limit : 100;
    const items: unknown[] = [];
    const typeFilter = typeof args.memory_type === "string"
      ? `&memory_type=${encodeURIComponent(args.memory_type)}` : "";
    let cursor: string | null = null;
    for (let page = 0; page < this.maxListPages && items.length < limit; page++) {
      const pageSize = Math.min(limit - items.length, API_LIST_PAGE_MAX);
      const query = cursor
        ? `limit=${pageSize}&cursor=${encodeURIComponent(cursor)}`
        : `limit=${pageSize}`;
      const data = await this.get(`/v1/memories?${query}${typeFilter}`);
      const rec = this.asRecord(data);
      const pageItems = Array.isArray(rec?.items) ? rec!.items : Array.isArray(data) ? data : null;
      if (pageItems === null) {
        throw new Error("Invalid Memoria API list page: expected an items array");
      }
      const rawCursor = rec?.next_cursor;
      if (rawCursor !== null && rawCursor !== undefined && typeof rawCursor !== "string") {
        throw new Error("Invalid Memoria API list page: next_cursor must be a string or null");
      }
      const next = typeof rawCursor === "string" && rawCursor ? rawCursor : null;
      // A cursor that does not advance means the server repeated a page: drop it
      // so rows are not counted twice, and stop so a misbehaving server cannot
      // loop us. The leftover cursor still marks the result as incomplete.
      if (next !== null && next === cursor) {
        break;
      }
      items.push(...pageItems);
      cursor = next;
      if (!next || pageItems.length === 0) {
        break;
      }
    }
    return { items: items.slice(0, limit), has_more: cursor !== null || items.length > limit };
  }

  /** Returns the active memory or null; the API answers `null` for unknown IDs. */
  private async getOne(args: Record<string, unknown>) {
    const data = await this.get(this.memoryPath(String(args.memory_id)));
    return this.asRecord(data) ?? null;
  }

  private async profile() {
    const data = await this.get("/v1/profiles/me");
    const rec = this.asRecord(data);
    if (rec?.profile && typeof rec.profile === "string") {
      return rec.profile;
    }
    return "No profile memories found.";
  }

  private async correct(args: Record<string, unknown>) {
    // Correct by ID or by query
    if (args.memory_id && typeof args.memory_id === "string") {
      const body: Record<string, unknown> = {
        new_content: args.new_content,
      };
      if (args.reason) body.reason = args.reason;
      const data = await this.put(this.memoryPath(args.memory_id, "/correct"), body);
      const rec = this.asRecord(data);
      return `Corrected memory ${rec?.memory_id ?? args.memory_id}: ${rec?.content ?? args.new_content}`;
    }
    // Correct by query
    const body: Record<string, unknown> = {
      query: args.query,
      new_content: args.new_content,
    };
    if (args.reason) body.reason = args.reason;
    const data = await this.post("/v1/memories/correct", body);
    const rec = this.asRecord(data);
    return `Corrected memory ${rec?.memory_id ?? ""}: ${rec?.content ?? args.new_content}`;
  }

  private async purge(args: Record<string, unknown>) {
    const body: Record<string, unknown> = {};
    if (args.memory_id !== undefined && args.memory_id !== null) {
      if (typeof args.memory_id !== "string") {
        throw new Error("memory_id must be a string");
      }
      // Match the MCP single-ID / comma-separated batch selector contract.
      const ids = args.memory_id.split(",").map((id) => id.trim()).filter(Boolean);
      // The API treats an empty array as no ID selector. Never let an invalid
      // batch silently fall through to a broader topic deletion.
      if (ids.length === 0) {
        throw new Error("memory_id must contain at least one non-empty ID");
      }
      body.memory_ids = ids;
    }
    if (args.topic) body.topic = args.topic;
    if (args.reason) body.reason = args.reason;
    const data = await this.post("/v1/memories/purge", body);
    const rec = this.asRecord(data);
    const count = typeof rec?.purged === "number" ? rec.purged : 0;
    return `Purged ${count} memory(ies).`;
  }

  private async observe(args: Record<string, unknown>) {
    const body: Record<string, unknown> = {
      messages: args.messages,
    };
    if (args.session_id) body.session_id = args.session_id;
    const data = await this.post("/v1/observe", body);
    return JSON.stringify(data);
  }

  // ── Governance / Graph ──────────────────────────────────────────

  private async governance(args: Record<string, unknown>) {
    return this.post("/v1/governance", { force: args.force ?? false });
  }

  private async consolidate(args: Record<string, unknown>) {
    return this.post("/v1/consolidate", { force: args.force ?? false });
  }

  private async reflect(args: Record<string, unknown>) {
    return this.post("/v1/reflect", {
      force: args.force ?? false,
      mode: args.mode ?? "auto",
    });
  }

  private async extractEntities(args: Record<string, unknown>) {
    return this.post("/v1/extract-entities", { mode: args.mode ?? "auto" });
  }

  private async linkEntities(args: Record<string, unknown>) {
    let entities = args.entities;
    if (typeof entities === "string") {
      try { entities = JSON.parse(entities); } catch { /* keep as-is */ }
    }
    return this.post("/v1/extract-entities/link", { entities });
  }

  // ── Snapshots ─────────────────────────────────────────────────

  private async createSnapshot(args: Record<string, unknown>) {
    const body: Record<string, unknown> = { name: args.name };
    if (args.description) body.description = args.description;
    const data = await this.post("/v1/snapshots", body);
    const rec = this.asRecord(data);
    const name = rec?.name ?? args.name ?? "";
    const ts = rec?.timestamp ?? rec?.created_at ?? "";
    return `Snapshot '${name}' created at ${ts}`;
  }

  private async listSnapshots(args: Record<string, unknown>) {
    const limit = typeof args.limit === "number" ? args.limit : 20;
    let offset = typeof args.offset === "number" ? args.offset : 0;
    // Explicit transport-level pagination remains a single page. The public
    // client/tool supplies neither argument and promises all known snapshots.
    if (typeof args.limit === "number" || typeof args.offset === "number") {
      return this.formatSnapshotList(await this.get(`/v1/snapshots?limit=${limit}&offset=${offset}`));
    }
    const snapshots: unknown[] = [];
    for (let page = 0; page < this.maxListPages; page++) {
      // Discovery only uses snapshot metadata, so avoid historical count backfills.
      const data = await this.get(`/v1/snapshots?limit=${limit}&offset=${offset}&resolve_memory_counts=false`);
      const rec = this.asRecord(data);
      if (!Array.isArray(rec?.snapshots)) {
        // Preserve older unpaginated response formats on the first request.
        if (page === 0) return this.formatSnapshotList(data);
        throw new Error("Memoria snapshot listing returned a malformed page");
      }
      const items = rec.snapshots;
      snapshots.push(...items);
      if (rec.has_more !== true) return this.formatSnapshotList({ snapshots });
      if (items.length === 0) throw new Error("Memoria snapshot listing returned an empty page with has_more");
      offset += items.length;
    }
    // Do not let listSnapshots/stats present a bounded prefix as a complete list.
    throw new Error(`Memoria snapshot listing exceeded maxListPages (${this.maxListPages})`);
  }

  private async rollbackSnapshot(args: Record<string, unknown>) {
    return this.post(`/v1/snapshots/${encodeURIComponent(String(args.name))}/rollback`, {});
  }

  // ── Branches ──────────────────────────────────────────────────

  private async branchCreate(args: Record<string, unknown>) {
    const body: Record<string, unknown> = { name: args.name };
    if (args.from_snapshot) body.from_snapshot = args.from_snapshot;
    if (args.from_timestamp) body.from_timestamp = args.from_timestamp;
    return this.post("/v1/branches", body);
  }

  private async branchList() {
    return this.get("/v1/branches");
  }

  private async branchCheckout(args: Record<string, unknown>) {
    return this.post(`/v1/branches/${encodeURIComponent(String(args.name))}/checkout`, {});
  }

  private async branchDelete(args: Record<string, unknown>) {
    return this.delete(`/v1/branches/${encodeURIComponent(String(args.name))}`);
  }

  private async branchMerge(args: Record<string, unknown>) {
    const body: Record<string, unknown> = {};
    if (args.strategy) body.strategy = args.strategy;
    return this.post(`/v1/branches/${encodeURIComponent(String(args.source))}/merge`, body);
  }

  private async branchDiff(args: Record<string, unknown>) {
    const limit = typeof args.limit === "number" ? args.limit : 50;
    return this.get(`/v1/branches/${encodeURIComponent(String(args.source))}/diff?limit=${limit}`);
  }

  // ── Health ──────────────────────────────────────────────────────

  async healthCheck(): Promise<{ status: string; instance_id?: string; db?: boolean }> {
    const data = await this.get("/health/instance");
    const rec = this.asRecord(data);
    const status = typeof rec?.status === "string" ? rec.status.trim() : "";
    if (!status) {
      throw new Error("Invalid health response: missing status");
    }
    return {
      status,
      instance_id: typeof rec?.instance_id === "string" ? rec.instance_id : undefined,
      db: typeof rec?.db === "boolean" ? rec.db : undefined,
    };
  }

  // ── HTTP primitives ───────────────────────────────────────────

  private memoryPath(id: string, suffix = ""): string {
    // WHATWG URLs normalize even percent-encoded dot-only segments as navigation.
    if (id === "." || id === "..") {
      throw new Error("memory_id must not be a dot-only path segment");
    }
    return `/v1/memories/${encodeURIComponent(id)}${suffix}`;
  }

  private headers(): Record<string, string> {
    // In api mode, the API key (sk-...) already scopes to its owning user.
    // Sending X-User-Id is unnecessary and can cause cross-user leakage
    // in open-auth deployments. Let the API key determine identity.
    return {
      "Authorization": `Bearer ${this.apiKey}`,
      "Content-Type": "application/json",
      "X-Memoria-Tool": "openclaw",
    };
  }

  private async request(method: string, path: string, body?: unknown): Promise<unknown> {
    const url = `${this.apiUrl}${path}`;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);

    try {
      const response = await fetch(url, {
        method,
        // Redirects can replay private memory bodies to another origin, or
        // rewrite a mutation into a GET that is falsely reported as success.
        redirect: "error",
        headers: this.headers(),
        body: body !== undefined ? JSON.stringify(body) : undefined,
        signal: controller.signal,
      });

      const text = await response.text();

      if (!response.ok) {
        throw new Error(
          `Memoria API ${method} ${path} returned ${response.status}: ${text.slice(0, 500)}`,
        );
      }

      if (!text.trim()) {
        return { ok: true };
      }

      try {
        return JSON.parse(text);
      } catch {
        return text;
      }
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") {
        throw new Error(`Memoria API request timed out after ${this.timeoutMs}ms: ${method} ${path}`);
      }
      throw error;
    } finally {
      clearTimeout(timer);
    }
  }

  private get(path: string) {
    return this.request("GET", path);
  }

  private post(path: string, body: unknown) {
    return this.request("POST", path, body);
  }

  private put(path: string, body: unknown) {
    return this.request("PUT", path, body);
  }

  private delete(path: string) {
    return this.request("DELETE", path);
  }

  // ── Formatting helpers ────────────────────────────────────────
  // Preserve structured API memory fields such as retrieval_score. The client
  // also understands the Rust MCP binary's legacy text format.

  private formatMemoryList(data: unknown): string {
    // The retrieve/search endpoints return an array or a results/items envelope.
    const rec = this.asRecord(data);
    if (Array.isArray(data)) {
      return this.formatMemoryItems(data);
    }
    if (Array.isArray(rec?.results)) {
      return this.formatMemoryItems(rec.results);
    }
    if (Array.isArray(rec?.items)) {
      return this.formatMemoryItems(rec.items);
    }
    throw new Error(
      "Invalid Memoria API memory list response: expected an array or an object with a results/items array",
    );
  }

  private formatMemoryItems(items: unknown[]): string {
    return JSON.stringify(items);
  }

  private formatSnapshotList(data: unknown): string {
    // Produce text matching Rust MCP format: "Snapshots (N):\n  name (timestamp)\n  ..."
    const rec = this.asRecord(data);
    const items = rec?.result ? this.asRecord(rec.result) : rec;
    // The API may return an array or an object with items/snapshots
    let snapshots: unknown[] = [];
    if (Array.isArray(data)) {
      snapshots = data;
    } else if (items && Array.isArray(items)) {
      snapshots = items;
    } else if (rec) {
      // Try common response shapes
      if (Array.isArray(rec.snapshots)) snapshots = rec.snapshots;
      else if (Array.isArray(rec.items)) snapshots = rec.items;
      else if (typeof rec.result === "string") return rec.result as string;
    }
    if (snapshots.length === 0) {
      return "Snapshots (0):";
    }
    const lines = [`Snapshots (${snapshots.length}):`];
    for (const snap of snapshots) {
      const s = this.asRecord(snap);
      if (!s) continue;
      const name = s.name ?? s.snapshot_name ?? "";
      const ts = s.timestamp ?? s.created_at ?? "";
      lines.push(`  ${name} (${ts})`);
    }
    return lines.join("\n");
  }

  private formatBranchList(data: unknown): string {
    // Produce text matching Rust MCP format: "Branches:\n  name\n  name ← active"
    const rec = this.asRecord(data);
    let branches: unknown[] = [];
    if (Array.isArray(data)) {
      branches = data;
    } else if (rec) {
      if (Array.isArray(rec.branches)) branches = rec.branches;
      else if (Array.isArray(rec.items)) branches = rec.items;
      else if (typeof rec.result === "string") return rec.result as string;
      else throw new Error("Invalid Memoria API branch list response: expected branches/items array");
    } else {
      throw new Error("Invalid Memoria API branch list response: expected branches/items array");
    }
    if (branches.length === 0) {
      return "Branches:\n  main ← active";
    }
    const lines = ["Branches:"];
    for (const branch of branches) {
      const b = this.asRecord(branch);
      if (!b) continue;
      const name = typeof b.name === "string" ? b.name : "";
      const active = b.active === true;
      lines.push(`  ${name}${active ? " ← active" : ""}`);
    }
    return lines.join("\n");
  }

  private asRecord(value: unknown): Record<string, unknown> | null {
    return value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  }
}
