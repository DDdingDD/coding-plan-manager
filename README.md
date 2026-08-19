# Coding Plan Manager

管理多个 AI Coding Plan（如 GLM / Claude / OpenAI 兼容的编码套餐），并将它们聚合成一个统一的对外端点。外部客户端（Claude Code、Cline、Cursor 等）只需配置一个 BASE_URL 和 AUTH_TOKEN，系统按**阈值轮转策略**自动把请求转发到内部绑定的各个 Coding Plan 上，同时统计 token 消耗、记录全部交互消息。

```
┌──────────────────────────────────────────────────────────────┐
│                     Tauri 桌面应用                             │
│   前端 Vue3 + Vite + ant-design-vue  ⇄  Rust 核心              │
│   · Coding Plan 管理  · 聚合器管理/启停  · 消息查看  · 统计      │
│                                        SQLite (rusqlite)      │
└──────────────────────────────────────────────────────────────┘
        每个"运行中"的聚合器各起一个 Axum 服务（独立端口，可启停）

外部客户端 (Claude Code / Cline / Cursor …)
      │  BASE_URL = http://127.0.0.1:{port}   AUTH_TOKEN = 自动生成
      ▼
┌───────────────────────┐  ①阈值轮转选 plan     ┌──────────────────┐
│  Axum 反向代理          │ ───────────────────► │ Coding Plan 上游  │
│  鉴权 -> 转发 -> 流式透传  │  替换为 plan 的令牌   │ (Anthropic/OpenAI │
│  用量解析 -> 落库 -> 推送UI│ ◄─────────────────── │  兼容 API)        │
└───────────────────────┘       响应             └──────────────────┘
```

## 功能

- **Coding Plan 管理**：配置名称 / BASE_URL / AUTH_TOKEN / 备注，可启用禁用
- **Plan Aggregator**：一个聚合器绑定多个 Coding Plan；创建时自动生成对外 AUTH_TOKEN 与端口
- **转发策略（阈值轮转）**：按绑定顺序使用计划；某计划累计消耗达到阈值后自动切到下一个；**全部达到阈值后自动清零回绕**到第一个重新计数
- **透明代理**：任意 method/path 转发（`/v1/messages`、`/v1/chat/completions`、`count_tokens`…均可）；`Authorization: Bearer` 与 `x-api-key` 双头兼容；`anthropic-version` / `anthropic-beta` 等头原样透传；SSE 流式响应边转发边统计
- **统计**：全局概览与按天 / 按小时 / 按模型的 token 用量（输入/输出/缓存）及请求数，支持按聚合器和时间段（全部/近 7/14/30 天）筛选；侧边栏另有小计里程式分段统计（token/请求数），可随时手动重置，不影响累计值
- **系统托盘**：关闭窗口默认隐藏到托盘（左键单击恢复），可在侧边栏改为退出前弹确认框；托盘菜单可退出程序
- **消息记录**：每条转发请求的完整请求体 / 响应体（超 2MB 截断）、状态码、耗时、token 用量，实时推送 UI
- **服务启停**：随时启动 / 停止聚合器，端口冲突友好报错

## 开发

前置要求：Node 18+、Rust（MSVC toolchain）、VS C++ Build Tools。

```bash
npm install
npm run tauri dev      # 开发
npm run tauri build    # 打包
cargo test --manifest-path src-tauri/Cargo.toml   # 后端测试（单测 + e2e）
```

### 验收用模拟上游

```bash
node scripts/mock-upstream.mjs 9401
# 模拟 Anthropic / OpenAI 两种格式，JSON 与 SSE 流式均支持，
# 会打印收到的鉴权头用于确认"替换为 plan 令牌"是否生效
```

## 客户端接入

以 Claude Code 为例（在 UI 启动聚合器后，从列表复制 BASE_URL 与 AUTH_TOKEN）：

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8300"
$env:ANTHROPIC_AUTH_TOKEN = "cpm-xxxxxxxx"
claude
```

持久化写入 `~/.claude/settings.json`：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8300",
    "ANTHROPIC_AUTH_TOKEN": "cpm-xxxxxxxx"
  }
}
```

OpenAI 兼容客户端同理设置 `OPENAI_BASE_URL` / `OPENAI_API_KEY` 指向聚合器。

上游 Coding Plan 示例（GLM Anthropic 兼容端点）：

| 配置项 | 值 |
|---|---|
| BASE_URL | `https://open.bigmodel.cn/api/anthropic` |
| AUTH_TOKEN | 你的 GLM API Key |

## 已知边界

- 应用重启后聚合器不会自动恢复运行，需在界面手动启动（规划中）
- 上游响应若不包含 usage 字段，该条消息 token 记 0（不估算）
- 请求体入库上限 10MB，响应体 2MB（超出截断并标记）
- 数据库文件位于系统应用数据目录 `cpm.db`
