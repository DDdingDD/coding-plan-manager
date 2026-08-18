# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目

Coding Plan Manager：Tauri 2 桌面应用。管理多个 AI Coding Plan（GLM/Claude/OpenAI 兼容上游），并将其聚合成统一对外端点。每个"运行中"的聚合器在 Tauri 进程内启动一个独立的 Axum 反向代理服务；外部客户端（Claude Code 等）将 `ANTHROPIC_BASE_URL` 指向聚合器，代理按阈值轮转策略把请求转发给绑定的计划并替换鉴权令牌。

## 常用命令

```bash
npm install                              # 前端依赖
npm run tauri dev                        # 开发（Vite dev server + Tauri 窗口）
npm run build                            # 前端 vue-tsc 类型检查 + Vite 构建
npm run tauri build                      # 打包桌面应用

# Rust 测试（在仓库根目录执行；需 MSVC 工具链）
cargo test --manifest-path src-tauri/Cargo.toml              # 全部：25 单测 + 11 e2e
cargo test --manifest-path src-tauri/Cargo.toml --lib        # 仅单元测试
cargo test --manifest-path src-tauri/Cargo.toml --test e2e   # 仅端到端测试
cargo test --manifest-path src-tauri/Cargo.toml --test e2e e2e_tcp_server_start_stop  # 单个测试

node scripts/mock-upstream.mjs 9401      # 本地模拟上游（Anthropic/OpenAI 格式，JSON+SSE）
```

数据库文件在 `%APPDATA%\com.codingplan.manager\cpm.db`（SQLite/WAL）。调试 UI 时删除该文件可重置全部配置。

## 架构

前端 `src/`（Vue 3 + ant-design-vue，四个页面：计划/聚合器/消息/统计），后端 `src-tauri/src/`。核心在 Rust 侧：

- **请求转发主链路** `proxy/handler.rs`（兜底路由，任意 method/path）：校验聚合器令牌（Bearer/x-api-key 均可）→ `strategy.rs` 选计划 → reqwest 转发（剥跳段头/原鉴权头，注入所选计划的 AUTH_TOKEN 为 Bearer + x-api-key 双头）→ 响应回传（SSE 走 mpsc channel 流式透传并累积副本；JSON 整体回传）→ `usage.rs` 解析用量（OpenAI/Anthropic 的 JSON 与 SSE 格式，Anthropic message_start 的 usage 嵌套在 `message.usage`；SSE 用量由 `SseUsageTracker` 在转发途中按行增量解析、不依赖 2MB 落库截断副本——否则流尾部的 message_delta/最终 chunk 丢失会少计 output_tokens；同时解析 prompt caching 的 `cache_read_input_tokens`/`cache_creation_input_tokens`，Anthropic 无显式 total 时按原始口径合计四项 input + cache_read + cache_creation + output，因此驱动轮转的绑定用量含缓存部分；OpenAI 的 `prompt_tokens_details.cached_tokens` 是 prompt 子集，刻意不解析以免重复计数）→ 落库 + 累加绑定用量 + emit `message:new` 事件推前端。
- **阈值轮转** `proxy/strategy.rs`：按绑定顺序取第一个"启用且 used_tokens < token_threshold"的计划；**全部超额时清零所有绑定并回绕到第一个**（需求语义，勿改成 503）；仅"无任何启用计划"返回 503。
- **服务生命周期** `proxy/mod.rs::start_server`：bind TcpListener → spawn axum（CancellationToken 优雅关停）。运行中的服务句柄存 `state.rs AppState.servers`（仅内存，应用重启后不自动恢复）。
- **Tauri 命令层** `commands/`（plans/aggregators/messages）：thin wrapper，每个命令自行 `lock()` 拿连接；统计查询命令（global/daily/hourly/model_stats）在 messages.rs。
- **系统托盘** `tray.rs`：左键单击/菜单恢复主窗口；关闭窗口默认隐藏到托盘，侧边栏开关可改为退出确认（偏好存 localStorage，逻辑在 `App.vue`）。
- **单连接模型**：整个应用共享一个 `Arc<Mutex<rusqlite::Connection>>`（commands 与代理服务共用）。handler **每次请求从 DB 重读聚合器配置**，因此阈值/令牌修改即时生效，无需重启服务；仅改端口要求先停止。

**统计的双轨设计（有意为之）**：`messages` 表 SUM 是历史统计（清零回绕不影响）；`aggregator_plans.used_tokens` 是轮转状态（回绕时清零）。测试断言时注意两者差异。

## 关键坑（改代码前必读）

1. **tauri 已禁用 `common-controls-v6` 默认特性**（见 Cargo.toml 注释）：该特性静态导入仅存在于 comctl32 v6 的 `TaskDialogIndirect`，而 cargo 测试二进制没有 tauri-build 嵌入的 SxS manifest，会以 `STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139)` 启动崩溃。不要"恢复默认特性"。
2. **前端 invoke 参数键名必须 camelCase**（如 `aggregatorId`/`baseUrl`/`tokenThreshold`）：tauri v2 宏把 Rust snake_case 参数转成 camelCase 后查找，无 snake_case 回退。Rust 命令参数侧保持 snake_case；serde 返回结构保持 snake_case 字段（`src/types.ts` 镜像）。
3. e2e 测试（`src-tauri/tests/e2e.rs`）通过 `proxy::build_router` + tower oneshot 驱动代理，mock 上游为真实 TCP；SSE 分支的落库在后台任务完成，断言前用 `wait_for_messages` 轮询。

## 约定

- 界面与代码注释使用中文；Rust 错误消息面向用户返回中文。
- 转发路径透传业务头（`anthropic-version`/`anthropic-beta` 等），新增头处理时注意维护 `SKIP_REQ_HEADERS`/`SKIP_RESP_HEADERS` 两个跳过清单。
- 上限：请求体转发 32MB（对齐 Anthropic 请求上限，带截图的大请求不被误拒）；请求/响应体落库均 2MB（`cap_store` 截断标记）。
