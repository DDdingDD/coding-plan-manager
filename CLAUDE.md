# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目

Coding Plan Manager：Tauri 2 桌面应用。管理多个 AI Coding Plan（GLM/Claude/OpenAI 兼容上游），并将其聚合成统一对外端点。每个"运行中"的聚合器在 Tauri 进程内启动一个独立的 Axum 反向代理服务；外部客户端（Claude Code 等）将 `ANTHROPIC_BASE_URL` 指向聚合器，代理按路由策略（阈值轮转/模型匹配）把请求转发给绑定的计划并替换鉴权令牌。计划可配置「支持模型」列表（可空），模型匹配策略据此路由。

## 常用命令

```bash
npm install                              # 前端依赖
npm run tauri dev                        # 开发（Vite dev server + Tauri 窗口）
npm run build                            # 前端 vue-tsc 类型检查 + Vite 构建
npm run tauri build                      # 打包桌面应用

# Rust 测试（在仓库根目录执行；需 MSVC 工具链）
cargo test --manifest-path src-tauri/Cargo.toml              # 全部：82 单测 + 13 e2e
cargo test --manifest-path src-tauri/Cargo.toml --lib        # 仅单元测试
cargo test --manifest-path src-tauri/Cargo.toml --test e2e   # 仅端到端测试
cargo test --manifest-path src-tauri/Cargo.toml --test e2e e2e_tcp_server_start_stop  # 单个测试

# 真实库副本上的「迁移 → 回填压缩 → 逐行哈希比对 → 整理 → 再比对」（默认忽略，耗时按副本体积而定；
# 路径含 com.codingplan.manager 一律拒绝）。副本用 scripts/copy-db.py 从真实库只读复制
python scripts/copy-db.py tail "%APPDATA%/com.codingplan.manager/cpm.db" D:/cpm-work/cpm-tail.db 5000
CPM_REAL_DB_COPY=D:/cpm-work/cpm-tail.db cargo test --manifest-path src-tauri/Cargo.toml --test real_db -- --ignored --nocapture
# 全量副本同一测试约 40 分钟（其中两轮逐行解码比对占一半）；应用正在运行时别用默认 target 目录做
# release 构建/测试：cargo 要连带重写 target\release\coding-plan-manager.exe，被占用即失败（os error 5），
# 换 CARGO_TARGET_DIR=D:/cpm-work/target 之类的独立目录，也别去结束正在运行的实例

node scripts/mock-upstream.mjs 9401      # 本地模拟上游（Anthropic/OpenAI 格式，JSON+SSE）
```

数据库文件在 `%APPDATA%\com.codingplan.manager\cpm.db`（SQLite/WAL，已是数十 GB 量级的全部历史消息）。调试 UI 时删除该文件可重置全部配置；要对着真实数据调试就设 `CPM_DB_PATH` 指向副本（仅 debug 构建生效：`CPM_DB_PATH=D:\cpm-work\cpm.db npm run tauri dev`）。涉及存储格式、迁移、维护任务的改动一律只在副本上验证，不动真实库。

## 架构

前端 `src/`（Vue 3 + ant-design-vue，五个页面：计划/聚合器/消息/统计/配置），后端 `src-tauri/src/`。核心在 Rust 侧：

- **请求转发主链路** `proxy/handler.rs`（兜底路由，任意 method/path）：校验聚合器令牌（Bearer/x-api-key 均可）→ `strategy.rs` 选计划 → reqwest 转发（剥跳段头/原鉴权头，注入所选计划的 AUTH_TOKEN 为 Bearer + x-api-key 双头）→ 响应回传（SSE 走 mpsc channel 流式透传并累积副本；JSON 整体回传）→ `usage.rs` 解析用量（OpenAI/Anthropic 的 JSON 与 SSE 格式，Anthropic message_start 的 usage 嵌套在 `message.usage`；SSE 用量由 `SseUsageTracker` 在转发途中按行增量解析、不依赖 2MB 落库截断副本——否则流尾部的 message_delta/最终 chunk 丢失会少计 output_tokens；同时解析 prompt caching 的 `cache_read_input_tokens`/`cache_creation_input_tokens`，Anthropic 无显式 total 时按原始口径合计四项 input + cache_read + cache_creation + output，因此驱动轮转的绑定用量含缓存部分；OpenAI 的 `prompt_tokens_details.cached_tokens` 是 prompt 子集，刻意不解析以免重复计数）→ 落库 + 累加绑定用量 + emit `message:new` 事件推前端（载荷为不含 body 的 `MessageSummary`，由 `insert_message` 返回的 id/created_at 加 `lookup_names` 拼出，不回读大字段行；前端各监听点只把它当刷新触发器，防抖 1000~1500ms）。
- **路由策略** `proxy/strategy.rs`：`pick_plan(conn, agg, model)` 按 `aggregators.strategy` 分派，`peek_plan` 以相同规则**无副作用**推导「当前转发的计划」（`AggregatorView.current_plan_id`，供前端高亮；不得改用 `pick_plan`--阈值轮转会触发清零/清除失效指定、模型匹配会写 current_plan_id），改路由规则时两处必须同步：
  - `threshold_rotation` 阈值轮转：手动指定的当前计划（current_plan_id 非空且「已绑定+启用+未达阈值」）优先固定转发；指定失效（达阈值/禁用/解绑）时 pick **自动清除指定**并恢复轮转。未指定时按绑定顺序取第一个"启用且 used_tokens < token_threshold"的计划；**全部超额时清零所有绑定并回绕到第一个**（需求语义，勿改成 503）；仅"无任何启用计划"返回 503（两种策略同此语义）。
  - `model_match` 模型匹配：handler 从请求体提取模型名传入（落库副本被 2MB 截断时用完整 body 重试）。匹配集=「绑定且启用且 `coding_plans.models` 含该模型」的计划（models 逗号分隔存储，空=不参与匹配，仅可作当前计划承接流量）。当前计划在匹配集中则不变（粘性，即使别家用量更少）；否则切到匹配集中 used_tokens 最少者（同量取先绑定）并把新计划持久化为当前；无匹配（含未携带模型）走当前计划；当前失效（未绑定/禁用）回退第一个启用绑定并固化。**token 阈值不参与该策略路由**，used_tokens 仅用于展示与"最少用量"比较。当前计划存 `aggregators.current_plan_id`（`Aggregator` 结构体上标注 `#[serde(skip)]`--AggregatorView 以 flatten 内嵌 Aggregator 且自带派生的 current_plan_id 与 DB 原值 manual_current_plan_id--前端用它区分「自动轮转/已固定」，同时序列化会产生重复 JSON 键）。pick 不自查 DB 里的 current_plan_id，依赖 handler 每请求重读配置；命令层 `set_current_plan` 两种策略均可用（须为绑定且启用的计划；plan_id 传 null 清除指定），显式切换策略（update_aggregator 检测到 strategy 变化）会清除 current_plan_id。
- **服务生命周期** `proxy/mod.rs::start_server`：bind TcpListener → spawn axum（CancellationToken 优雅关停）。运行中的服务句柄存 `state.rs AppState.servers`（仅内存，应用重启后不自动恢复）。
- **Tauri 命令层** `commands/`（plans/aggregators/messages）：thin wrapper，plans/aggregators 的命令自行 `lock()` 拿连接；messages.rs 的命令全部为 async 并经 `run_db`（clone 连接 Arc + `spawn_blocking`）执行——Tauri v2 同步命令在主线程跑、慢查询会冻结 UI。统计查询命令（global/daily/hourly/model_stats 及小计里程 trip_stats/reset_trip/toggle_trip_pause）在 messages.rs；统计 SQL 必须命中 `idx_messages_stat_*` 覆盖索引（body 列在记录中部，回表即遍历溢出页链，数十 GB 级库上全表扫是分钟级），改统计 SQL/索引以 `stats_queries_hit_covering_indexes` 单测把关（EXPLAIN QUERY PLAN 断言），索引定义演进走 `ensure_index`（归一化比较 sqlite_master 的 DDL，不一致则 DROP 重建）。小计里程类似汽车短途里程：`settings` KV 表存起点时间（`trip_started_at`），按 `created_at>=起点` 聚合 messages，重置仅改起点、不动消息数据，故与累计统计天然互不影响；未重置时（起点为 NULL）小计=全部历史。暂停功能：`trip_paused_at` 存在即暂停（统计冻结在该时刻），继续时把 `[paused_at, now)` 追加到 `trip_pauses` JSON 区间列表，查询用半开区间排除——暂停期间的用量恢复后不补计；重置会一并清除两个暂停键。前端侧边栏圆盘组件 `src/components/TripGauge.vue`：单击=暂停/继续（250ms 延迟区分双击）、双击=重置（无确认框），暂停态琥珀色/计数中蓝色。`list_messages` 返回不含请求/响应体的 `MessageSummary`（两个 body 各上限 2MB，列表页不展示、不值得搬运；详情走 `get_message` 拉全量 `MessageRow`）。新建聚合器自动分配端口时跳过 DB 已占用端口（`resolve_create_port`，服务未运行时 bind 测不出冲突）。**配置导入导出** `commands/config.rs`：`export_config`/`import_config` 同步命令 + rfd 原生文件对话框（rfd 走 IFileDialog COM，不依赖 comctl32 v6，与坑 #1 无关；刻意不引入 tauri 插件/capabilities 改动）。导出文件用文件内 `ref_id`/`plan_refs`/`current_plan_ref` 引用而非自增 id（跨库导入时 id 必变）；聚合器 `auth_token` 原样导出/导入（客户端 `ANTHROPIC_AUTH_TOKEN` 指向它，必须保留才能复现）；`used_tokens`/`created_at`/运行态刻意不导出（配置 vs 运行状态边界）。导入事务包裹整体回滚；merge=追加+端口冲突自动改配（`find_free_port`）并报告明细，replace=先停全部服务（fire-and-forget cancel，不等待排水——`delete_aggregator` 的 5s await 同样不保证端口释放）再 `db::clear_config` 重建（messages/settings 保留）；「当前计划」恢复刻意走 db 层 `set_current_plan`（绕过命令层「绑定且启用」校验，忠实复现含禁用计划的指定）。
- **消息体存储编码** `body.rs` + `db.rs`（`insert_message`/`get_message`/`clear_messages`/`compress_batch`）：`messages.request_body/response_body` 沿用原列名，按**存储类**区分形态——`typeof='text'` 是旧库明文、`'blob'` 是编码态。BLOB = 1 字节标签 + 负载：`0x01` = `u32 LE 原始长度` + 带校验和的 zstd 帧；`0x02` = 原始字节（zstd 不比原长短、或压缩上下文建不出来时的兜底，**编码永不失败**）；空体 = 空 BLOB。请求体可存**前缀增量**（同一会话每轮重发全部历史，相邻请求体是前缀关系，81% 的请求体字节是此前某条的前缀）：`req_ref_id` 指向来源行、`req_prefix_len` 为取自来源行请求体的前缀字节数，本行只存后缀；还原时自本行回溯到整条行（跳数上限 `MAX_REF_HOPS=256`）再逐层拼接，响应体一律整条。**引用不变量：`req_ref_id` 只指向同一 `aggregator_id` 的行**——按聚合器清空消息删掉的必是完整引用闭包，幸存行无需改写。来源行在 `BodyStore`（进程内近期请求体环：每聚合器 128 条/64MB，合计 128MB、最多 8 个聚合器 LRU；只用于写入时选候选，读取只依赖库内数据）里选：公共前缀 ≥ `MIN_PREFIX`(16KB) 且 ≥ body 的 1/4、来源深度 < `MAX_DEPTH`(32)，否则存整条；**锁序恒为 DB → BodyStore**（选候选与 DELETE 同处一个 DB 锁区间，来源行落库瞬间必然存在，写前还 `ref_exists` 复核）。压缩级别：在线 `ONLINE_LEVEL=3`（耗时落在请求收尾的持锁区间内），回填 `BACKFILL_LEVEL=12`。部分索引 `idx_messages_legacy ON messages(id) WHERE typeof(request_body)='text'`：新行是 BLOB 不进索引（零写放大）；「是否还有未压缩行」O(1)、`legacy_message_count` 只数索引项、回填扫描走 `SEARCH (id>?)`。只有 `get_message` 读 body（`list_messages`/统计不碰）；读路径对引用断裂/数据损坏**降级不报错**：正文以 `…[引用的请求体 #id 已不存在，前 N 字节无法恢复]` 之类中文标记开头，其后是仍可还原的部分。还原一条增量行 = 沿链最多 32 次主键查询 + 逐层解压拼接，开销与引用深度成正比（单条详情毫秒级），别把它搬进列表/统计这类成批读的路径。
- **库维护任务** `maintenance.rs`（不依赖 tauri，可单测）：「压缩历史数据」（历史行回填）与「整理文件」（VACUUM）都由用户在配置页**手动触发**，没有自动压缩/自动清理。同一时刻只跑一个任务（命名线程 `cpm-maintenance`，`try_start` 拒绝第二个）；状态记在模块自己的 Mutex，进度查询与取消都不取 DB 锁，`get_db_info` 用 `try_lock`（整理期间拿不到锁时页级统计字段为 null，文件大小与任务状态照常），前端有任务时每秒轮询 `get_db_info`。回填 `run_compress`：循环「取锁 → `compress_batch`（整批一个事务、幂等、任务私有的 `BodyStore`）→ 放锁」，取消在批间生效，已提交的行保持有效、其余仍是明文，再次压缩续跑；批量自适应——有聚合器运行时 ≤8 行/≤4MiB + 批间歇 50ms（代理请求最多等一批），没有时 ≤4096 行/≤16MiB 连跑（级别 12 实测约 18 分钟压完 32GB、体积降到 8% 左右；旧库首启的补列 + 建部分索引只要 1 秒内——`typeof()` 走 `OPFLAG_TYPEOFARG`，不读大字段）。整理 `db::vacuum` 须先停全部聚合器（VACUUM 全程独占连接，运行中的代理请求会在锁上挂起几分钟而不是排队）：前端按钮置灰 + `start_vacuum` 后端校验，`start_aggregator` 在整理期间拒绝启动；**耗时只与有效数据量成正比、与文件大小无关**（实测约 25 秒/GiB，32GB 文件里只有 2.6GB 有效数据 → 65 秒），界面的时长/临时空间估算都按 `estimated_size` 算；中断（`progress_handler` 读取消标志 → `SQLITE_INTERRUPT`）是 SQLite 的原子回滚，返回 `Ok(false)`，但最后的回写阶段不经过 VM、不触发进度回调，取消要等该阶段写完（十几秒）才生效。
- **系统托盘** `tray.rs`：左键单击/菜单恢复主窗口；关闭窗口默认隐藏到托盘，配置页开关可改为退出确认（偏好存 localStorage，逻辑在 `App.vue`）。
- **单连接模型**：整个应用共享一个 `Arc<Mutex<rusqlite::Connection>>`（commands 与代理服务共用）。handler **每次请求从 DB 重读聚合器配置**，因此阈值/令牌/策略修改即时生效，无需重启服务；仅改端口要求先停止。模型匹配的粘性当前计划与阈值轮转的手动固定也依赖该重读保持新鲜（pick_plan 信任传入的 agg 结构体）。

**统计的双轨设计（有意为之）**：`messages` 表 SUM 是历史统计（清零回绕不影响）；`aggregator_plans.used_tokens` 是轮转状态（回绕时清零）。测试断言时注意两者差异。

## 关键坑（改代码前必读）

1. **tauri 已禁用 `common-controls-v6` 默认特性**（见 Cargo.toml 注释）：该特性静态导入仅存在于 comctl32 v6 的 `TaskDialogIndirect`，而 cargo 测试二进制没有 tauri-build 嵌入的 SxS manifest，会以 `STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139)` 启动崩溃。不要"恢复默认特性"。
2. **前端 invoke 参数键名必须 camelCase**（如 `aggregatorId`/`baseUrl`/`tokenThreshold`）：tauri v2 宏把 Rust snake_case 参数转成 camelCase 后查找，无 snake_case 回退。Rust 命令参数侧保持 snake_case；serde 返回结构保持 snake_case 字段（`src/types.ts` 镜像）。
3. e2e 测试（`src-tauri/tests/e2e.rs`）通过 `proxy::build_router` + tower oneshot 驱动代理，mock 上游为真实 TCP；SSE 分支的落库在后台任务完成，断言前用 `wait_for_messages` 轮询。
4. **`temp_store=MEMORY` 下 VACUUM 的临时库会整个进内存**：连接常态是内存临时存储（`init_schema`），VACUUM 要先把新库完整写进临时库，不改就是把整个新库放进 RAM。`db::vacuum` 先切 `temp_store=FILE`、任何出口都恢复。新增会落大临时文件的语句（数十 GB 表上 `CREATE INDEX`、大结果集排序）同样先确认临时存储落盘。
5. **body 列只能按原始字节读**：`row.get::<_, String>` 遇 BLOB 报 `InvalidColumnType`，读 `request_body/response_body` 一律 `row.get_ref(i)` 取 `ValueRef` 的字节（`StoredBody::from_row`）再解码。直接 `SELECT request_body` 拿到的是编码态/增量后缀而不是原文——要原文走 `get_message`；统计 SQL 只对数值列求和，不受影响。
6. **任何删除 `messages` 行的新路径都必须保住引用闭包**：增量行的前缀取自来源行，来源行被删则前缀不可恢复（读时降级为标记 + 可得后缀，内容部分丢失）。现有路径靠「同聚合器不变量」成立（按聚合器清空/全部清空）。若新增按日期、按计划等删除方式，必须同步处理受影响的增量行（级联删除或改写为整条），并在**同一个 DB 锁区间内**清掉 `BodyStore` 里对应条目（`remove_aggregator`/`clear`），否则之后的新行可能引用已删除的行。

## 约定

- 界面与代码注释使用中文；Rust 错误消息面向用户返回中文。
- 转发路径透传业务头（`anthropic-version`/`anthropic-beta` 等），新增头处理时注意维护 `SKIP_REQ_HEADERS`/`SKIP_RESP_HEADERS` 两个跳过清单。
- 上限：请求体转发 32MB（对齐 Anthropic 请求上限，带截图的大请求不被误拒）；请求/响应体落库均 2MB（`cap_store` 截断标记）。
- 后台任务单次持锁控制在 ~250ms 内（有聚合器运行时小批量 + 批间歇），代理请求在 DB 锁上最多等一批；需要独占连接的操作（VACUUM）以「先停全部聚合器」为前置条件。
- 消息体存储格式只增不改：动 `body.rs` 的标签/阈值/`MAX_DEPTH` 前先想旧库兼容，读路径必须永远能解出（或降级）已落库的任何形态；`BodyStore::plan_delta` 是在线落库与历史回填共用的唯一选候选规则，别写第二份。
