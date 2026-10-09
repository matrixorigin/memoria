# Memoria Hermes 记忆插件

当前是 **0.1.1 开发预览版**。默认连接免费的 Memoria Cloud，也支持自托管 API。
用户无需自行部署数据库、embedding 模型或事实提取服务。

0.1.0 已通过真实 Cloud API 验收；0.1.1 的去重捕获需要先部署配套服务端更新，
再更新插件并补做真实 Cloud 验收。目前尚未进入 Hermes 官方插件目录。
暂不要把 `hermes plugins install memoria` 作为已上线命令。

同轮调用 `memoria_store` 或 `memoria_update` 成功后，自动捕获仅从当前轮工具结果
提取已保存记忆的 ID，发送至 `/v1/observe/deduplicated`。服务端在相同账户、branch、
subject 下读取这些事实，让提取模型排除它们的同义表述和翻译，同时保留其他新事实；
代码另行过滤完全相同的内容，包括脱敏后才变为相同的内容。跳过的精确重复不计入
observe 响应的 `memories`，不会返回未落库候选的 ID。语义排除仍依赖模型遵循提示词。
排除 ID 也会保护向量去重阶段的原始记录：最近邻命中被排除的记录且最终内容不同时，插入新候选，
不取代该记录，也不改为寻找次近记录取代。这会保留不同的新事实；如果模型违反提示
返回同义改写，仍可能新增重复记录。成功的 store/update 始终返回简短回执，仅包含
记忆 ID、subject，以及服务端提供的有效记忆类型，不回显内容或元数据。回执小于
已验证宿主的预览大小和单条结果预算。若整轮预算仍使回执变成 `<persisted-output>`，
捕获只解析该格式预览中完整的回执 JSON；忽略截断预览，不读取所引用的文件，
并继续校验当前轮工具调用关联及 subject。
已失效的记录仍提供排除内容，避免更新／删除后被积压捕获重新写回；不存在或跨账户、
跨主体的 ID 会被忽略，不读取其内容，也不会阻断整轮捕获。
缺少新路由响应标记的 404 会报告 `capture_dedup_endpoint_unavailable`，保留 failed
记录，需检查 API 版本及代理路由后手动重试；分支不存在等带标记的业务 404 仍为
`not_found`。插件不会回退到普通 observe 造成重复；
带排除条件时，未配置 LLM 或提取失败也不会退回原文存储。升级顺序是服务端在先、
插件在后。已有重复记录及旧版队列内容不会被本次更新自动修改或删除。

旧 `/v1/observe` 有意兼容排除字段，服务层错误仍保持原来的 500 映射。新路由返回
明确的业务状态码，并附带 `X-Memoria-Observe-Deduplicated: 1`。持久化前的提取失败
返回 503 及 `X-Memoria-Observe-Error: extraction_unavailable`，插件对此自动退避
重试；其他 5xx 仍按写入结果未知处理。工具结果识别仅支持已验证 Hermes 使用的
OpenAI `tool_calls`／`tool` 格式，不支持 Anthropic `tool_result` 或轮次中插入合成
user 消息的格式。

## 安装和登录

要求 Hermes ≥ 0.21.5、Python ≥ 3.11；实际验证的 Hermes 提交为
`0240fa4a84123406a0e5e6e7262e5b772b43f0bd`。同版本号的不同开发构建可能有接口差异，
应在目标环境执行插件校验。

从 Memoria 仓库安装到默认 Hermes profile：

```sh
mkdir -p ~/.hermes/plugins
cp -R plugins/hermes ~/.hermes/plugins/memoria
hermes plugins validate ~/.hermes/plugins/memoria --install-deps
hermes plugins enable memoria
hermes memory setup
hermes memory status
```

若已有同名目录，先检查再更新。命名 profile 请使用它的实际 `$HERMES_HOME`，并在该
profile 下运行命令，不能统一复制到默认目录。

设置时选择 **Memoria**，登录 [Memoria 官网](https://thememoria.ai)，创建或复制
记忆服务 API Key，粘贴到向导。现有官网登录流程可以继续使用；网站登录会话 token
不能代替 API Key。插件目前没有实现浏览器登录后自动授权 Hermes。

向导会询问是否开启自动捕获：输入 `true` 开启、`false` 关闭，默认关闭。开启后会把
新增的用户／助手文本发往 Memoria 提取记忆；工具结果、图片和系统提示词不会自动上传。
设置完成后新建 Hermes 会话。

## 已实现的能力

- 自动召回：相关性检索、查询／会话独立缓存、后台预热、上下文长度限制。
- 六个工具：`memoria_search`、`memoria_store`、`memoria_update`、`memoria_forget`、
  `memoria_profile`、`memoria_feedback`。
- 自动捕获：仅新增轮次，持久待提交队列，重复回调去重，故障状态可检查。
- 隔离：按 profile 目录生成稳定主体；检索、画像及按 ID 修改／删除均检查主体。
- 会话切换：新任务使用新会话，已经入队的任务保持原会话归属。
- 写入审批：Hermes 的 `memory.write_approval` 开启时暂停外部写入，避免绕过审批。

本版限定本地 CLI／桌面对话。网关多人场景尚未验收，运行时禁用；subagent、cron、
flush 等非主上下文不自动捕获，也不能执行修改工具。主体筛选是账号内的逻辑隔离，
不能替代服务端授权。同一账号的不同 API Key 不一定对应不同授权范围。

移动或重命名 profile 目录会生成新主体，不会自动合并旧记忆。修改、删除使用确切 ID；
修正操作可能返回新的 ID。画像工具支持 `cursor`／`limit` 分页，每页 1–50 条。
超出输出预算时保留已容纳记录和可继续查询的游标；单条过长时保留 ID 和明确标记的内容摘录。

## 自托管及高级配置

非敏感配置位于 `$HERMES_HOME/memoria.json`，例如：

```json
{
  "api_url": "https://api.thememoria.ai",
  "branch": "main",
  "auto_recall": true,
  "auto_capture": false,
  "top_k": 5,
  "context_chars": 6000,
  "recall_timeout": 2.0,
  "request_timeout": 15.0,
  "max_capture_chars": 24000,
  "queue_capacity": 1000
}
```

自托管修改 `api_url` 即可，使用 HTTPS origin，不附加 `/v1`。本机开发可用
`http://localhost:8100`。API Key 仍通过当前 profile 的 Hermes 凭据机制提供。
每次请求明确携带分支，不改变账号共享 checkout。反馈接口目前只支持 `main`。
更改配置后重新启动会话。

冷缓存仍在宿主的外部 provider 线程中同步查询，默认 2 秒 IO 超时，短于已验证 Hermes
的 8 秒等待预算。自定义时也应保持插件超时小于宿主预算。IO 超时不是总耗时上限，
慢速流式响应可能超出宿主等待，期间宿主会抑制重叠召回。

Hermes 当前会把插件结果包在“权威参考数据”的提示中，与插件内部“不可信数据、不可
执行指令”的说明存在冲突。插件无法修改外层包装，因此没有承诺可完全防止记忆提示词注入。

## 故障和恢复

队列保存在 `$HERMES_HOME/plugin-data/memoria/outbox.sqlite3`，与插件安装目录分开。
待提交或失败记录包含上传文本，目录／文件权限为私有；完成后清除文本，只保留去重
指纹和状态。卸载插件保留这些数据；不再需要时请明确删除。

连接建立失败、HTTP 429 及带明确标记的写入前提取失败留在 pending，
按 1、2、4……秒指数退避，间隔最多 300 秒，
不限制次数；持久保存下次尝试时间，重启后继续。服务端 `Retry-After` 可延长等待，最多
24 小时。发送后的超时、其他 HTTP 5xx、异常成功响应或中断
提交标记为 `uncertain`，不自动重发。普通 HTTP 拒绝进入 `failed`。崩溃时未结束的
提交在 120 秒租约到期后转为未知。尚未发送的 pending 记录在同一绑定恢复、自动捕获
开启且无需审批时继续处理。

绑定包含 API 地址、凭据哈希、主体及分支。换 Key、换分支或移动 profile 后，旧绑定的
队列不会自动通过新配置发送；启动会提示旧绑定尚有任务，诊断工具可显示各绑定。
容量只统计当前绑定的 pending／inflight；failed、uncertain 和旧绑定不会耗尽当前
绑定额度，但仍需管理其本地数据占用。当前自动队列满时明确跳过新捕获。

SQLite 短暂锁冲突会退避，不会结束 worker；远端已有结果时只重试本地状态写回，不再
重发请求。完成写回同时检查 inflight 状态及每次领取任务的 token，迟到结果不能覆盖
人工丢弃／重试或一次新的领取。关闭期间若写回仍被锁住，记录可能留下 inflight，之后
转为 uncertain。升级前停止旧版插件进程；SQLite schema 会在事务中迁移。

检查状态，不显示对话正文：

```sh
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes
```

先在 Cloud 检查写入是否已经发生，再手工重试或丢弃：

```sh
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes --retry <EVENT_ID> --acknowledge-duplicate-risk
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes --discard <EVENT_ID>
```

手工重试可能产生重复记忆。当前服务端尚未持久实现 observe 的事件幂等，因此本版
不承诺 exactly-once。宿主未提供历史时，同一会话的相同用户／助手文本对按重复事件处理。
`observe_raw_fallback` 表示服务端报告没有提取模型、直接保存原文；提取故障也可能
退化为原文保存，但当前 API 不一定返回单独警告。
明确未发送／被拒绝的记录可不带 `--acknowledge-duplicate-risk` 手工重试；结果未知的
记录仍需该参数。重试 pending 记录会重置等待时间。损坏的 `memoria.json` 可通过设置
向导修复：先保留权限 0600 的 `.invalid-*` 副本，再写入经校验的新配置。

## 验证和发布状态

首版已通过 39 项自动测试；review 后新增并扩大回归测试，结果见
[修复记录](REVIEW_FIXES.md)。测试使用真实 Hermes MemoryManager／凭据作用域搭配
HTTP 模拟接口；完成了官方插件校验、插件宿主进程隔离加载、设置向导、状态展示以及
卸载保留数据检查。2026-10-08 使用用户提供的 Key 完成 13 项真实 Cloud API 检查：
保存、跨会话召回、默认超时内自动召回、画像、双 profile 隔离、反馈、修正、删除、
自动捕获及重复回调去重全部通过。测试主体下剩余活跃记忆为 0。

本次通过真实 Hermes MemoryManager 调用插件和线上 API，会话切换通过宿主钩子模拟，
尚未运行完整自然语言 Agent 聊天验收。详细记录见 [Cloud 验收记录](CLOUD_ACCEPTANCE.md)。

公开发布前需发布包含插件的真实 commit，验证 Git 安装命令，再向 Hermes 官方目录
提交固定 SHA 的条目。官网 Hermes
接入入口应在安装命令可用之后开放。详见 [发布清单](RELEASE.md)。

原生 MEMORY.md／USER.md 镜像、网关作者映射、会话总结、快照／回滚工具、服务端
事件幂等和 checkpoint v2 完整证据归档尚未实现。

完整参数与测试命令见 [英文 README](README.md)。插件直接使用 REST，Python SDK
的新参数补齐可以独立发布，不阻塞此预览版本。
