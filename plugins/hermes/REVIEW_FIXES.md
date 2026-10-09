# Review 修复记录

本轮处理用户转交的 7 项确认缺陷及相关契约问题，仍只修改 Memoria 仓库内插件代码和文档。
没有修改服务端幂等、Hermes 核心或官网。本变更作为 Memoria PR 提交审阅，
尚未发布 GitHub release、包或 Hermes catalog 条目。

| Review 项 | 修改 |
| --- | --- |
| 1. SQLite 短暂锁导致 worker 永久退出 | 在循环内捕获 OperationalError 并退避；把远端结果暂存为 completion，数据库写回重试不触发网络重发 |
| 2. 离线很快耗尽三次重试，人工队列长期积累 | 持久 `next_attempt_at`，连接失败无限次指数退避，最多 300 秒间隔；保存 `failure_kind` 区分未发送／拒绝／未知；仅 pending／inflight 计入自动队列容量 |
| 3. 旧绑定占满新绑定容量且不可见 | 容量按 binding 统计，启动提示其他绑定的未完成任务；旧任务不通过新凭据自动发送 |
| 4. 迟到检索写回已清空缓存 | 失效时递增 generation；后台预热及冷查询仅在版本未变时回填，冷查询也不返回已失效结果 |
| 5. 迟到 finish 覆盖人工操作／新领取 | 同时检查 inflight 及每次领取的唯一 claim_token；零行更新视为 superseded，不能覆盖后来状态 |
| 6. 网络恢复后相同错误不再提示 | 成功请求清除错误去重状态，恢复后再故障会再次提示 |
| 7. observe 的 429 直接失败 | 429 作为可安全重试的拒绝，遵循 Retry-After（最多 24 小时），与连接失败一样持久排期 |

SQLite 自动迁移新增的 `next_attempt_at`、`failure_kind` 和 `claim_token` 列；旧版明确的
connection_failed／rate_limited 失败转为可重试 pending，未知结果不自动重放。升级应停止
旧进程，避免旧版无 token 的写回逻辑继续操作同一数据库。

另已处理：

- profile 工具增加 cursor／limit，超长页保留可读内容及后续游标；单条过长明确标注摘录。
- 历史中非 dict 项被跳过；显式内容同时校验 UTF-8 32 KiB 上限。
- 初始化通过 ExitStack 管理客户端，部分失败立即关闭已构造的客户端。
- initialize 的显式 home 必须匹配当前 profile context，避免检查配置与实际写入目录分离。
- 设置向导可把损坏配置备份为私有 `.invalid-*` 文件后修复。

保留并说明的取舍：

- 冷查询仍同步运行于宿主外部 provider 线程。默认插件 IO timeout 为 2 秒，固定宿主等待为
  8 秒；自定义时必须保持前者较短。HTTP IO timeout 不是总 wall-clock deadline，未改变
  宿主对仍在运行的召回线程的重叠抑制行为。
- Hermes 外层把记忆标为 authoritative reference data，与插件内部不可信数据说明冲突；
  已写入 README，没有修改上游包装，也没有对外发送 issue／PR。
- 服务端事件幂等、严格 checkpoint v2、网关及其他延期能力仍未实现。
- 本轮没有解决所有磁盘容量管理：完成收据及 failed／uncertain 正文仍需诊断／保留策略。

Python 3.11.15 与 3.12.13 各通过 **65 项离线测试**，Ruff check／format check 通过。
GitHub Actions 尚未实际运行，以上是本机独立 venv 的复验。回归用例位于
`tests/test_reliability.py`，并更新了原有离线恢复及诊断测试；真实 Cloud 脚本仍为显式运行。

修复后再次通过 Hermes 官方 `plugins validate --install-deps`，以及 13 项真实 Cloud
API 检查；自动捕获确认 done、无 fallback warning，测试主体剩余活跃记忆为 0。
临时 profile 的插件宿主隔离加载也再次通过（HostedMemoriaMemoryProvider），配置 schema、
六个工具及初始化／会话切换／关闭均正常；该隔离 smoke 不执行网络调用。
