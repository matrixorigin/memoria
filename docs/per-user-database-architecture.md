# Per-User Database 架构、迁移与部署说明

> **状态**: Implemented and validated
> **作者**: Copilot + ghs-mo
> **最近更新**: 2026-04-06
> **适用范围**: `memoria-storage`, `memoria-git`, `memoria-service`, `memoria-mcp`, `memoria-api`, `memoria-cli`

---

## 1. 结论先行

Memoria 已从“所有用户共享一个数据库、靠 `user_id` 行级过滤隔离”的旧模型，演进为：

1. 一个 **shared DB** 承载全局控制平面数据。
2. 一个用户一个 **per-user DB** 承载该用户的业务数据与用户级控制元数据。
3. 运行时主路径采用 **global user pool + qualified table names**，而不是为每个用户维护长期连接池；shared DB 上的 shared/router/git 组件也合并复用同一个固定池。

关于 multi-db `/metrics` 的下一阶段长期设计，见：
`docs/metrics-rollup-refresh-design.md`

这个改造的核心收益不是“查询更快”，而是把 **snapshot / branch / restore / rollback** 从“天然会误伤所有用户”的危险操作，收敛成“只作用于当前用户数据库”的安全操作。

代价也很明确：

1. 数据库对象数量会上升，运维和观测复杂度高于单库模型。
2. `/admin/stats`、治理遍历这类全局聚合仍然要经过用户注册表；`/metrics` 则需要改成 shared DB summary + 异步刷新，不能把 per-user fan-out 放在 scrape 路径里。
3. 部署时必须明确 shared DB 的名字和 URL；不能再依赖“默认猜一个库名”。

---

## 2. 为什么要改

旧架构的问题不是“共享库不优雅”，而是 **Git-for-Data 语义在共享库里不成立**。

旧模型下，`mem_memories`、`memory_graph_*`、`mem_edit_log` 等核心表都混在一个数据库里。这样会导致：

1. **rollback 风险不可接受**  
   一个用户执行 restore / rollback，本质上是对全表做恢复；如果表里混着所有用户的数据，那么恢复操作天然是全局性的。

2. **snapshot 边界错误**  
   snapshot 如果基于 account 或共享数据库创建，天然包含其他用户状态；用户 A 的“回到我的某个时刻”会变成“把整库带回某个时刻”。

3. **branch / diff 只能靠应用层过滤**  
   数据分支和 diff 如果以 account 或共享表为边界，Rust 层再按 `user_id` 过滤，本质上只是补丁，不是隔离。

因此，per-user DB 不是“性能优化”，而是 **把版本控制语义落到正确的数据边界上**。

---

## 3. 最终落地架构

### 3.1 数据库布局

```text
MatrixOne account
|
+-- memoria_shared*                 # shared DB（名字由部署决定，不要求固定）
|   +-- mem_user_registry
|   +-- mem_api_keys
|   +-- mem_distributed_locks
|   +-- mem_async_tasks
|   +-- mem_governance_runtime_state
|   +-- mem_plugin_*
|
+-- mem_u_<hash(user_id)>
|   +-- mem_memories
|   +-- mem_memories_stats
|   +-- mem_branches
|   +-- mem_snapshots
|   +-- mem_user_state
|   +-- mem_edit_log
|   +-- mem_entities
|   +-- mem_entity_links
|   +-- mem_memory_entity_links
|   +-- memory_graph_nodes
|   +-- memory_graph_edges
|   +-- mem_retrieval_feedback
|   +-- mem_user_retrieval_params
|   +-- mem_governance_cooldown
|   +-- mem_tool_usage
|   +-- mem_api_call_log
|   +-- br_*
|
+-- mem_u_<hash(other_user)>
    +-- ...
```

### 3.2 shared DB 放什么

shared DB 只放 **跨用户共享、跨实例协调、或必须全局查询** 的控制平面数据：

| 表 | 为什么必须留在 shared DB |
|---|---|
| `mem_user_registry` | `user_id -> db_name` 路由事实源 |
| `mem_api_keys` | API key 校验必须跨用户查询 |
| `mem_distributed_locks` | 分布式锁天然是全局控制面 |
| `mem_async_tasks` | 跨实例异步任务状态 |
| `mem_governance_runtime_state` | 治理熔断与运行态 |
| `mem_plugin_*` | 插件包、签名者、绑定、审计均为全局能力 |

### 3.3 per-user DB 放什么

per-user DB 放两类数据：

1. **业务数据**：记忆、图谱、反馈、调用日志、分支表。
2. **用户级控制元数据**：`mem_branches`、`mem_snapshots`、`mem_user_state`、`mem_governance_cooldown`。

注意：这些“用户级控制元数据”虽然在用户库里，但它们不是 rollback 的业务恢复目标；它们要保持当前态，否则会出现 branch / snapshot 注册表与底层事实不一致。

### 3.4 鉴权与首次建库的边界

API Key 校验只依赖 shared DB。缓存命中仍须检查 Key 的 `expires_at`（UTC）；缓存 TTL 不能延长 Key 的有效期。无到期时间的 Key 继续遵守正常缓存 TTL。

MCP 的工具权限检查必须先于群组存储访问、工具使用统计和个人调用日志入队。因 scope 不足而拒绝的请求（包括 notification），以及格式错误的请求，不得为了记录拒绝而创建个人记忆库。这些事件保留在运行日志中，并在启用 shared stats reporter 时计入共享汇总统计。

仅有 `identity:read` 的 Key 调用身份或 MCP 元数据接口时，也不写入可能触发建库的个人统计；即使带有 `X-Memoria-Tool` 或 `X-Tool-Name` Header，行为不变。获准的记忆工具调用仍正常记录使用情况，并可按原有流程首次建库。以上边界不需要新增表或数据库迁移。

---

## 4. 最终实现选择：global user pool + qualified tables

这次真正落地的不是最初 RFC 里那种“长期 per-user pool + 请求期 `USE db` 切换”方案，而是下面这个版本：

1. `DbRouter` 维护：
   - 一个合并后的 **shared DB pool**（同时承载 router / shared store / git，默认 20）
   - 一个 **global user pool**（承载常规 per-user 读写，默认 80）
   - 一个 **init pool**（承载首次用户 schema bootstrap / compat migration，默认 12）
2. auth 保持独立 **auth pool**（默认 12），governance 仍保持小隔离池（默认 2）
3. 路由出来的 user store 本身携带 `db_name`
4. 用户表通过全限定名访问，例如：
   - `` `mem_u_53f19f9ab3e3d6f1`.mem_memories ``
   - `` `mem_u_53f19f9ab3e3d6f1`.memory_graph_nodes ``
5. shared 表继续走 shared DB 上下文，不做全限定用户库拼接

这套默认固定预算现在是 **20 + 80 + 12 + 12 + 2 = 126**，仍然留在 150 总连接预算以内。

### 为什么最终没有用 per-user pool

因为 per-user pool 在真实多用户场景下会把问题从“隔离”变成“连接爆炸”：

1. 用户数上来以后，连接数和空闲池数量线性增长。
2. LRU 缓存 per-user pool 只能减轻冷用户问题，不能消除冷启动抖动。
3. `USE db` 配合 statement cache、后台 worker、跨 shared/user 查询时会产生额外复杂性。

最终方案的取舍是：

| 方案 | 好处 | 问题 |
|---|---|---|
| per-user pool + `USE db` | 语义直接 | 连接爆炸、池管理复杂 |
| global user pool + qualified tables | 连接数稳定、shared/user 边界清晰 | SQL 需要系统性表名限定 |

这也是当前代码最终落地的版本。

---

## 5. Snapshot / Branch / Restore 的最终边界

### 5.1 用户 rollback 只回退业务数据

用户级 restore / rollback 的边界是：

| 域 | 典型表 | 是否纳入用户 restore |
|---|---|---|
| 业务数据域 | `mem_memories`, `memory_graph_nodes`, `memory_graph_edges`, `mem_edit_log` | 是 |
| 用户级控制元数据 | `mem_snapshots`, `mem_branches`, `mem_user_state`, `mem_governance_cooldown` | 否，保持当前态 |
| shared 控制平面 | `mem_user_registry`, `mem_api_keys`, `mem_distributed_locks`, `mem_async_tasks`, `mem_plugin_*` | 否，严禁用户 restore |

### 5.2 这样设计的原因

如果把 `mem_snapshots` / `mem_branches` / `mem_user_state` 一起回退，会出现：

1. 物理上还存在的 branch / snapshot，在控制平面里“消失”。
2. `active_branch` 静默切回旧值。
3. 快照配额、branch 配额、治理 cooldown 状态和真实系统状态脱节。

因此，**“per-user DB” 不等于 “整库用户随意 restore”**。  
真正安全的定义是：**业务数据按用户库隔离；控制平面保持当前态并做对账。**

### 5.3 Branch / Snapshot 的现实约束

当前实现里，迁移时会保留已有 `br_*` 物理表名，不做重命名重写。这样做的好处是：

1. 减少迁移时对 branch metadata 的额外修改。
2. 避免把“迁移用户数据”扩大成“迁移用户数据 + 重写所有 branch 元数据”。

代价是历史 `br_*` 名字不会因为 per-user DB 而变得更漂亮；这是有意接受的兼容性取舍。

---

## 6. 迁移流程（正式 runbook）

这一节描述的是 **旧单库 -> 新 shared DB + per-user DB** 的正式迁移流程。

### 6.1 迁移 CLI

当前正式命令是：

```bash
# dry-run（默认）
memoria migrate legacy-to-multi-db \
  --legacy-db-url mysql://user:pass@host:6001/memoria_legacy \
  --shared-db-url mysql://user:pass@host:6001/memoria_shared \
  --embedding-dim 1024 \
  --report-out migration-plan.json

# execute（真正执行）
memoria migrate legacy-to-multi-db \
  --legacy-db-url mysql://user:pass@host:6001/memoria_legacy \
  --shared-db-url mysql://user:pass@host:6001/memoria_shared \
  --embedding-dim 1024 \
  --concurrency 4 \
  --execute \
  --report-out migration-run.json
```

也支持：

```bash
memoria migrate legacy-to-multi-db \
  --legacy-db-url ... \
  --shared-db-url ... \
  --user alice \
  --user bob
```

这个 `--user` 主要用于 rehearsal / troubleshooting，不建议当正式灰度切流机制使用。

服务启动时的自动 legacy -> multi-db 迁移同样支持并发；默认并发为 `6`，可通过
`MEMORIA_LEGACY_MIGRATION_MAX_CONCURRENCY` 覆盖。若未设置该变量，则回落到
`MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY`。这个迁移仍然发生在服务对外就绪之前，
不会改成“边启动边迁移”的语义。

### 6.2 dry-run 做什么

dry-run 只做计划和风险检查，不写目标库。它应该回答四个问题：

1. 有多少用户会被迁移。
2. 每个用户会落到哪个 `mem_u_*`。
3. 有多少业务表、branch 表、共享表需要搬运。
4. 有没有 execute 前必须处理的阻断项。

### 6.3 execute 做什么

execute 的实际动作顺序是：

1. **创建 pre-execute account snapshot**
2. **reset 目标 shared DB**
3. **迁移 shared 持久表**
4. 按用户并发迁移：
   - 创建 / 重建 `mem_u_*`
   - 初始化用户库 schema
   - 复制用户业务表
   - 复制 `mem_memories_stats`
   - 复制该用户的 `br_*` 物理分支表
5. 写入 / 更新 `mem_user_registry`
6. 生成完整报告

### 6.4 execute 前的前置条件

1. **先 dry-run 一次**
2. **冻结写流量**
   - MCP 写入
   - REST 写入
   - 后台任务可能触发的数据写入
3. 保留 legacy DB，不要在 execute 前删库
4. 确认目标 shared DB 和目标 `mem_u_*` 没有你还要保留的生产数据

### 6.5 execute 后必须做的验证

最少要检查：

1. `mem_user_registry` 是否有完整用户映射。
2. 老 API key 是否仍可认证。
3. 关键老用户的 memories / branches / snapshots 是否可读。
4. brand-new user 是否能在新架构下首次自动建库并正常读写。
5. `/admin/users`、`/admin/stats`、`/metrics` 是否正常。
6. MCP `tools/call` 是否走到正确的 multi-db 服务。

### 6.6 回滚原则

如果 cutover 后出现严重问题：

1. 停止 multi-db 新服务。
2. 优先使用 execute 前生成的 **pre-execute account snapshot** 回滚。
3. 恢复旧配置，重新指向 legacy `DATABASE_URL`。
4. 不要立即销毁已生成的 shared / `mem_u_*`，先保留现场用于排障。

### 6.7 标准升级步骤：断流 -> 迁移 -> 部署新服务

这一节是给运维执行的 **cutover 操作手册**。目标不是解释“原理”，而是让你按顺序执行并知道每一步为什么存在。

> 适用前提：
>
> 1. 旧服务仍在 **single-DB / legacy DB** 上对外提供写服务。
> 2. 新服务将切到 **shared DB + per-user DB**。
> 3. 允许一次 **短暂写停机**。当前流程 **不支持 dual-write，也不支持不停写在线迁移**。

#### Step 0. 切流前预演：这些事不要放到断流窗口里做

先在正式 cutover 之前完成下面这些动作：

1. 编译或准备好新版二进制 / 镜像。
2. 用真实生产 URL 跑一次 **dry-run**。
3. 确认新版运行时配置已经准备好，但 **先不要接流量**。
4. 明确所有写入口：
   - 旧 Memoria API 实例
   - website backend（如果它会代理 `/api/chat` / `/api/memories` 并自动写记忆）
   - 任何直接调用 `/v1/*` 或 `/mcp` 的 agent / cron / batch job

推荐先准备统一变量：

```bash
export LEGACY_DB_URL='mysql://user:pass@host:6001/memoria_legacy_prod'
export SHARED_DB_URL='mysql://user:pass@host:6001/memoria_shared_prod'
export EMBEDDING_DIM=1024
export PORT=8116
export MASTER_KEY='replace-with-real-master-key'

export REPORT_PLAN='migration-plan.json'
export REPORT_RUN='migration-run.json'
```

先做 dry-run：

```bash
memoria migrate legacy-to-multi-db \
  --legacy-db-url "$LEGACY_DB_URL" \
  --shared-db-url "$SHARED_DB_URL" \
  --embedding-dim "$EMBEDDING_DIM" \
  --report-out "$REPORT_PLAN"
```

此时重点看四件事：

1. 用户总数是否符合预期。
2. 是否有 warnings / errors。
3. 目标 shared DB 名字是否正确。
4. 计划里生成的 `mem_u_*` 是否符合预期。

如果 dry-run 都不干净，不要进入 cutover。

#### Step 1. 预构建新版服务，但不要接流量

如果你走二进制部署：

```bash
cd /path/to/Memoria/memoria
source ~/.cargo/env
cargo build --release
```

如果你走镜像部署，也应在这里把镜像构建好并推到仓库，但 **不要 rollout**。

同时准备新服务运行时变量：

```bash
export DATABASE_URL="$SHARED_DB_URL"
export MEMORIA_MULTI_DB=1
export MEMORIA_SHARED_DATABASE_URL="$SHARED_DB_URL"

export EMBEDDING_PROVIDER='openai'
export EMBEDDING_BASE_URL='<embedding-endpoint>'
export EMBEDDING_API_KEY='<embedding-secret>'
export EMBEDDING_MODEL='<embedding-model>'
export EMBEDDING_DIM="$EMBEDDING_DIM"
```

如果还要保留 reflect / entity extraction / episodic 等 LLM 能力，再额外准备：

```bash
export LLM_BASE_URL='<llm-endpoint>'
export LLM_API_KEY='<llm-secret>'
export LLM_MODEL='<llm-model>'
```

如果 website backend 在 cutover 后也要继续工作，提前确认它的新配置：

- `memoria.api_url` -> 新 Memoria 服务 URL
- `memoria.master_key` -> 新服务使用的 master key

#### Step 2. 开始断流：先停所有写入口，再迁移

这一阶段的要求很简单：**任何能够写 memory / key / branch / snapshot 的入口，都必须先停掉。**

常见写入口包括：

1. 旧 Memoria API 实例本身。
2. website backend（因为它可能通过 `/api/chat` 自动提取并写记忆）。
3. 直接调用 `/mcp` 或 `/v1/memories` 的 agent / batch 任务。

根据你的部署方式，执行对应的停机动作：

```bash
# 本地/裸二进制
kill <old-memoria-pid>
kill <website-backend-pid>

# systemd
systemctl stop memoria
systemctl stop memoria-srv

# docker compose
docker compose stop memoria
docker compose stop memoria-srv

# Kubernetes
kubectl scale deploy/memoria --replicas=0 -n <ns>
kubectl scale deploy/memoria-srv --replicas=0 -n <ns>
```

如果你前面还有 API Gateway / LB，先把旧实例从对外流量里摘掉，再停进程。

#### Step 3. 确认断流真的生效

不要“感觉已经停了”就往下走。至少做下面两类确认：

1. **进程 / 服务侧确认**
   - 旧 Memoria 实例已从对外入口摘除，或进程已停止
   - website backend 不再可用，或者已切入维护状态

2. **数据侧确认**
   - 最近几秒内没有新的写请求进入旧库

如果 legacy 库里保留了 `mem_api_call_log`，可以直接看最近写请求：

```sql
SELECT method, path, status_code, called_at
FROM mem_api_call_log
ORDER BY called_at DESC
LIMIT 20;
```

你要确认的是：最近时间戳不再继续前进，尤其不要再出现新的 `POST` / `PUT` / `DELETE` 到 `/v1/*` 或 `/mcp/*`。

#### Step 4. 在断流窗口内执行正式迁移

真正执行时，用 `--execute`：

```bash
memoria migrate legacy-to-multi-db \
  --legacy-db-url "$LEGACY_DB_URL" \
  --shared-db-url "$SHARED_DB_URL" \
  --embedding-dim "$EMBEDDING_DIM" \
  --concurrency 4 \
  --execute \
  --report-out "$REPORT_RUN"
```

这里的关键语义是：

1. 迁移器会先创建 **pre-execute account snapshot**。
2. 然后重置目标 shared DB 并重建迁移结果。
3. 再把 shared 表和用户业务表搬到新的 shared / `mem_u_*`。

`--concurrency 4` 是 drill 中验证过的一个稳妥起点，不是硬编码要求。你的 DB 负载更高时，可以先保守，再逐步调大。

#### Step 5. 迁移完成后，先做离线校验，不要急着起新服务

先看迁移报告：

1. 有没有 errors。
2. warnings 是否都能解释。
3. 用户数、memory 数、API key 数是否在预期范围内。

然后做 shared DB 校验：

```sql
SELECT COUNT(*) AS routed_users FROM mem_user_registry;
SELECT user_id, db_name FROM mem_user_registry ORDER BY updated_at DESC LIMIT 20;
```

再抽样检查几个重要老用户，把 `db_name` 拿出来后，直接看用户库：

```sql
SELECT COUNT(*) AS memories FROM `<db_name-from-registry>`.mem_memories WHERE is_active = 1;
SELECT COUNT(*) AS branches FROM `<db_name-from-registry>`.mem_branches;
SELECT COUNT(*) AS snapshots FROM `<db_name-from-registry>`.mem_snapshots;
```

如果这一步就发现数据量明显不对，不要启动新服务；先保留现场，排查迁移报告和源库。

#### Step 6. 启动 multi-db 新服务

确认 Step 5 没问题后，再启动新版本：

```bash
export DATABASE_URL="$SHARED_DB_URL"
export MEMORIA_MULTI_DB=1
export MEMORIA_SHARED_DATABASE_URL="$SHARED_DB_URL"

export EMBEDDING_PROVIDER='openai'
export EMBEDDING_BASE_URL='<embedding-endpoint>'
export EMBEDDING_API_KEY='<embedding-secret>'
export EMBEDDING_MODEL='<embedding-model>'
export EMBEDDING_DIM="$EMBEDDING_DIM"

export LLM_BASE_URL='<llm-endpoint>'
export LLM_API_KEY='<llm-secret>'
export LLM_MODEL='<llm-model>'

memoria serve \
  --db-url "$DATABASE_URL" \
  --port "$PORT" \
  --master-key "$MASTER_KEY"
```

起服务后先只做基础存活检查：

```bash
curl -sf "http://127.0.0.1:${PORT}/health"
```

只有 health 正常后，才进入 API 烟测。

#### Step 7. 先验老用户，再验新用户

先用一个 **已迁移老用户的现有 API key** 验证读写：

```bash
export OLD_USER_KEY='sk-...'

curl -s "http://127.0.0.1:${PORT}/v1/memories?limit=3" \
  -H "Authorization: Bearer ${OLD_USER_KEY}"

curl -s -X POST "http://127.0.0.1:${PORT}/v1/memories/search" \
  -H "Authorization: Bearer ${OLD_USER_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"query":"smoke test","top_k":3}'
```

再做一个 **新用户** 烟测。最稳妥的方式是直接通过 master key 创建一把全新的 user key：

```bash
export NEW_USER_ID="cutover-smoke-$(date +%s)"

curl -s -X POST "http://127.0.0.1:${PORT}/auth/keys" \
  -H "Authorization: Bearer ${MASTER_KEY}" \
  -H "Content-Type: application/json" \
  -d "{\"user_id\":\"${NEW_USER_ID}\",\"name\":\"smoke\"}"
```

从返回结果里拿到 `raw_key` 后，执行一次确定性的写入：

```bash
export NEW_USER_KEY='sk-...'

curl -s -X POST "http://127.0.0.1:${PORT}/v1/memories" \
  -H "Authorization: Bearer ${NEW_USER_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"cutover smoke memory","memory_type":"semantic"}'
```

然后确认 shared DB 里出现了新路由，新用户库也真的落了数据：

```sql
SELECT user_id, db_name FROM mem_user_registry WHERE user_id = '<NEW_USER_ID>';
SELECT COUNT(*) FROM `<db_name-from-registry>`.mem_memories WHERE is_active = 1;
```

如果你线上还有 website backend，再做一次 website 侧烟测：

1. 把 website backend 的 `memoria.api_url` 改到新 Memoria URL。
2. 确认 `memoria.master_key` 与新服务一致。
3. 重启 website backend。
4. 验证 `/api/me`、`/api/keys`、`/api/memories`、`/api/chat`。

#### Step 8. 全部验证通过后再恢复流量

恢复顺序建议是：

1. 先恢复 website backend / 代理层。
2. 再恢复直接调用 Memoria 的 agent / job。
3. 最后恢复全部外部流量。

恢复后，持续盯这几类指标：

1. `/health` 是否稳定。
2. `/admin/users`、`/admin/stats`、`/metrics` 是否正常。
3. 新写入是否持续落到正确的 `mem_u_*`。
4. `/mcp` 的 `tools/call` 是否能正常写入和检索。

#### Step 9. 回滚步骤

如果 Step 6-8 任一阶段出现阻断问题，回滚顺序应该固定：

1. 停掉新 multi-db 服务。
2. 恢复旧 single-db 服务配置：
   - `DATABASE_URL` 指回 legacy DB
   - 关闭 `MEMORIA_MULTI_DB`
3. 如果 website backend 已切到新服务，把它的 `memoria.api_url` / `memoria.master_key` 改回旧值并重启。
4. 重新开放旧流量。
5. 保留 `REPORT_PLAN`、`REPORT_RUN`、新 shared DB、所有 `mem_u_*` 现场用于排障。

只有在确认问题已经定位后，才去处理目标库；不要在第一次故障现场里边切边删。

#### Step 10. 这个 runbook 何时不够用

这份 runbook 的边界也要说清楚：

1. 如果你要求 **零写停机**，当前方案不够用；因为它没有 dual-write 和增量 catch-up。
2. 如果 legacy 和 target 不在同一个 MatrixOne account，内置的 pre-execute account snapshot 只能覆盖它所在的 account；这时需要额外做源侧备份。
3. 如果 website 之外还有隐藏写入口，没有一起断流，迁移后的数据就会天然分叉。

---

## 7. 新服务镜像重启时必须设置的环境变量

这一节只讨论 **`memoria serve`** 的运行时环境变量，不包括测试变量、CLI 初始化变量、开发机专用变量。

### 7.1 迁移后重启的最小必需集合

如果你只是要让新版 multi-db 服务正常启动并工作，最小集合是：

> 最新运行时在**全新空环境**下，即使没显式设置 `MEMORIA_MULTI_DB`，也会自动按 multi-db 启动。
> 但生产环境仍然建议把 `MEMORIA_MULTI_DB=1` 和 `MEMORIA_SHARED_DATABASE_URL` 明确写出来，避免歧义，也兼容旧版本节点。

```bash
DATABASE_URL=<shared-db-url>
MEMORIA_MULTI_DB=1
MEMORIA_SHARED_DATABASE_URL=<shared-db-url>

EMBEDDING_PROVIDER=openai
EMBEDDING_BASE_URL=<your-embedding-endpoint>
EMBEDDING_API_KEY=<secret>
EMBEDDING_MODEL=<your-model>
EMBEDDING_DIM=<must-match-schema>
```

### 7.2 每个变量的作用

| 变量 | 是否迁移后重启必需 | 说明 |
|---|---|---|
| `DATABASE_URL` | 是 | multi-db 模式下应指向 shared DB |
| `MEMORIA_MULTI_DB` | 建议显式设置 | 置为 `1` / `true` 可强制打开新架构；最新运行时对 fresh empty 环境会自动切到 multi-db |
| `MEMORIA_SHARED_DATABASE_URL` | 是 | 显式告诉服务 shared DB 在哪；**不要依赖默认推导** |
| `EMBEDDING_PROVIDER` | 基本必需 | 不设会退回默认 `mock`，检索语义会变掉 |
| `EMBEDDING_BASE_URL` | 基本必需 | HTTP embedding 服务地址 |
| `EMBEDDING_API_KEY` | 基本必需 | embedding 鉴权 |
| `EMBEDDING_MODEL` | 基本必需 | 例如 `BAAI/bge-m3` |
| `EMBEDDING_DIM` | 基本必需 | 必须和库里向量维度一致 |

### 7.3 为什么 `MEMORIA_SHARED_DATABASE_URL` 不能省

代码在没拿到这个变量时，会尝试把 `DATABASE_URL` 的库名替换成固定的 `memoria_shared`。

这只在一种情况下安全：

1. 你的 shared DB 真的就叫 `memoria_shared`

但现实部署里，shared DB 往往会带环境后缀，例如：

- `memoria_shared_200u`
- `memoria_shared_prod`
- `memoria_shared_staging`

这种情况下，如果不显式设置 `MEMORIA_SHARED_DATABASE_URL`，服务就会猜错。

### 7.4 LLM 相关变量：不是启动必需，但可能是功能必需

如果你还要保留 reflect / entity extraction / episodic summary 等能力，需要再设置：

```bash
LLM_API_KEY=<secret>
LLM_BASE_URL=<llm-endpoint>
LLM_MODEL=<model-name>
```

如果这些功能不需要，可以不设；服务仍能启动，只是相关能力不可用或降级。

### 7.5 不是 cutover 必需的变量（可选调优）

下面这些不是“迁移后必须设”，而是部署调优项：

| 变量 | 用途 |
|---|---|
| `MEMORIA_MULTI_DB_POOL_BUDGET` | multi-db 连接池默认预算基线；默认 `512`，上限 `2048`；未显式配置的池按 `global/shared/auth/init/governance = 60/20/10/5/5` 推导 |
| `MEMORIA_GLOBAL_USER_POOL_MAX` | global user pool 大小 |
| `MEMORIA_LEGACY_MIGRATION_MAX_CONCURRENCY` | 启动期 legacy -> multi-db 自动迁移的用户并发上限 |
| `MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS` | 首次用户 schema init / compat migration 专用池大小 |
| `MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY` | 首次用户 schema init 并发上限 |
| `MEMORIA_ROUTER_INIT_TIMEOUT_SECS` | 用户/组冷初始化及用户 DB 映射查询的总时限（含 gate/全局许可等待），默认 120 秒，范围 1–3600 秒 |
| `MEMORIA_ROUTER_INVALIDATION_TIMEOUT_SECS` | 缓存失效等待总时限，默认 15 秒，范围 1–3600 秒；超时返回错误，不宣告失效成功 |
| `MEMORIA_SHARED_POOL_MAX_CONNECTIONS` | 兼容旧配置的 shared router 组件配额；仅在显式设置时参与 merged shared pool 计算 |
| `MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS` | 兼容旧配置的 shared store 组件配额；仅在显式设置时参与 merged shared pool 计算 |
| `MEMORIA_GIT_POOL_MAX_CONNECTIONS` | 兼容旧配置的 git 组件配额；仅在显式设置时参与 merged shared pool 计算 |
| `MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS` | 显式覆盖 merged shared pool 大小；优先级高于 budget 推导 |
| `MEMORIA_AUTH_POOL_MAX_CONNECTIONS` | auth 专用连接池大小 |
| `MEMORIA_AUTH_POOL_ACQUIRE_TIMEOUT_SECS` | auth 池获取超时 |
| `DB_MAX_LIFETIME_SECS` | DB 连接最大生命周期 |
| `ENTITY_QUEUE_SIZE` / `ENTITY_POOL_SIZE` / `ENTITY_WORKER_COUNT` | entity worker 调优 |
| `GRAPH_POOL_SIZE` | graph 检索隔离池大小 |
| `GOVERNANCE_POOL_SIZE` | 治理任务并发池 |
| `MEMORIA_METRICS_CACHE_TTL_SECS` | `/metrics` 缓存 TTL |
| `MEMORIA_RATE_LIMIT_AUTH_KEYS` | auth key rate limit |
| `MEMORIA_GOVERNANCE_ENABLED` | 治理开关 |
| `MEMORIA_GOVERNANCE_PLUGIN_BINDING` / `SUBJECT` / `DIR` | 治理插件配置 |
| `MEMORIA_INSTANCE_ID` | 分布式锁实例标识 |
| `MEMORIA_LOCK_TTL_SECS` | 分布式锁 TTL |
| `MEMORIA_SANDBOX_ENABLED` | pipeline sandbox 开关 |
| `EMBED_MAX_CONCURRENT` / `EMBED_SEMAPHORE_TIMEOUT_SECS` | embedding 并发限制 |

### 7.6 不是环境变量的启动参数

下面两个不是 env，而是启动参数：

```bash
memoria serve \
  --db-url "$DATABASE_URL" \
  --port 8116 \
  --master-key <master-key>
```

也就是说：

1. `port` 不是环境变量。
2. `master key` 不是环境变量。

### 7.7 一个完整但最小的 multi-db 启动示例

```bash
export DATABASE_URL='mysql://user:pass@host:6001/memoria_shared_prod'
export MEMORIA_MULTI_DB=1
export MEMORIA_SHARED_DATABASE_URL="$DATABASE_URL"

export EMBEDDING_PROVIDER='openai'
export EMBEDDING_BASE_URL='https://api.siliconflow.cn/v1'
export EMBEDDING_API_KEY='***'
export EMBEDDING_MODEL='BAAI/bge-m3'
export EMBEDDING_DIM=1024

export LLM_BASE_URL='https://api.openai.com/v1'
export LLM_API_KEY='***'
export LLM_MODEL='gpt-4o-mini'

memoria serve \
  --db-url "$DATABASE_URL" \
  --port 8116 \
  --master-key '<master-key>'
```

> 所有第三方密钥都属于运行时 secret，**必须由部署系统注入**，不能提交到仓库。

---

## 8. 当前实现里的关键工程取舍

### 8.1 保留 `user_id` 列

当前 per-user DB 里多数表仍保留 `user_id` 列。这不是因为物理隔离不够，而是为了：

1. 降低迁移和回退风险。
2. 保持部分逻辑与旧模型兼容。
3. 让迁移后的校验、审计和补救脚本更简单。

代价是 schema 还没有达到最简状态，但这是有意接受的 Phase 1 取舍。

### 8.2 shared 表永远不能被用户 restore

这是最重要的安全边界之一。  
如果用户 restore 能影响 shared DB，那么 API key、分布式锁、任务状态、用户路由都会被回滚，整个系统会从“用户级恢复”退化回“系统级破坏”。

### 8.3 全局聚合不再是单条 SQL 的问题

`/admin/stats`、治理遍历等能力，在 per-user DB 时代仍然需要经过：

1. `mem_user_registry`
2. 路由到用户库
3. 聚合结果

`/metrics` 不能直接走这条同步路径：Prometheus scrape 必须只读 shared DB summary。  
正确姿势是后台 worker 根据 `mem_user_registry` 找到 dirty 用户、路由进用户库重算 summary，再由 `/metrics` 读取 summary 表和进程内指标。

---

## 9. 当前已经验证过什么

这套架构已经在真实 drill 中完成过：

1. legacy single-db -> shared + per-user DB 迁移
2. 老用户迁移后登录、读写、检索、MCP 调用
3. 新用户 cutover 后自动建库与正常使用
4. branch / snapshot / rollback / merge 等全链路语义验证
5. website 代理链路与 MCP 链路验证

因此，这份文档不再是“候选方案说明”，而是 **当前实现 + 运维执行方式说明**。

---

## 10. PR / 部署边界说明

这次 per-user-per-db 改造本身发生在 **Memoria** 仓库。  
website 仓库里若有本地 debug 登录补丁，那只是 drill 辅助工具，不属于本架构变更的一部分。

---

## 11. 一句话运维原则

如果只记一条：

> **迁移后重启 multi-db 服务时，必须显式告诉它 shared DB 是谁；shared DB 只管控制平面，用户 rollback 只回退用户业务数据。**
