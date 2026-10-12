# Agent Web · Rust + tmux

A lightweight Rust web console for interactive coding agents, powered by tmux.

在浅色网页中管理本机 coding agent：创建、切换、关闭独立会话，保留真正的交互式 CLI、终端菜单和审批。Rust 提供 HTTP、WebSocket 和 PTY，tmux 在网页断开或后端重启后保留 agent 进程。

## 支持的 agent

| Agent | 默认启动顺序 | 网页聊天与活动 | 交互式终端 |
| --- | --- | --- | --- |
| Claude Code | `cc` → `claude` | 原生 JSONL 消息、工具活动、刷新恢复 | 支持 |
| Codex | `cx` → `codex` | 原生 JSONL 消息、工具活动、刷新恢复 | 支持 |
| 自定义 CLI，例如 Aider、Gemini CLI | `AGENT_BIN` + `AGENT_ARGS` | 尚无专用聊天解析器 | 支持 |

通用终端层可以接入其他交互式 CLI；不同 agent 的结构化聊天格式需要各自的适配器，不宣称所有 CLI 都已具备相同的聊天体验。

## 启动

需要 macOS / Linux、Rust、tmux，以及已安装并登录的目标 agent。

```bash
source "$HOME/.cargo/env"
cargo run
```

也可以使用 npm 包装器，它只负责构建和启动 Rust 程序：

```bash
npm start
```

打开 **http://127.0.0.1:3001**。新建会话前在左侧选择 agent；首次发送聊天消息也会自动创建会话。

启动默认 `AGENT=auto`：先检查 `cc` / `claude`，再检查 `cx` / `codex`。明确指定默认 agent：

```bash
AGENT=claude npm start
AGENT=codex npm start
AGENT_WORKDIR=/absolute/path/to/project PORT=3002 cargo run
```

shell 由系统决定：优先 `$SHELL`，其次账户的登录 shell，最后 `/bin/sh`。以登录、交互模式加载配置，支持 alias、shell 函数和可执行文件。已验证 zsh / Bash。Bash 登录配置需要正常加载 `.bashrc`，才能使用其中定义的函数。

```bash
cc() { claude "$@"; }
cx() { codex "$@"; }
```

只有找不到优先命令时才回退；命令自身报错不会自动启动第二个进程。系统 `/usr/bin/cc` 和 `/bin/cc` 是 C 编译器，会被跳过。检查只查找命令，不运行自定义函数的 `--version`。

`CLAUDE_BIN` / `CODEX_BIN` 可覆盖相应启动命令，`TMUX_BIN` 可指定 tmux。旧的 `CODEX_WORKDIR` 配置仍兼容。会话启动方式改变后，新建会话使用新配置，已有会话继续运行。

## 其他 coding agent

用命令名称和 JSON 参数数组接入其他交互式 CLI：

```bash
AGENT=custom AGENT_BIN=aider AGENT_LABEL=Aider AGENT_ARGS='[]' npm start
```

也可用 `AGENT_BIN=gemini` 或自己的 shell alias / 函数。参数应通过 `AGENT_ARGS` 逐项指定，`AGENT_BIN` 不接受整段 shell 命令。自定义 agent 使用终端模式；新建后自动打开终端面板，不向未知 CLI 发送 Codex 参数或猜测其聊天记录。

## 使用与会话生命周期

- 「新建会话」创建独立 tmux session 和对应 agent 进程，旧会话和任务继续运行。
- 左侧列表切换会话；消息、工具活动和未发送草稿各自保留。刷新记住上次选择的会话。
- 顶部「关闭会话」、列表旁的关闭按钮或终端面板「关闭当前会话」，确认后只停止指定 session 及其 agent / 任务。关闭后台会话不影响当前连接，原始 agent 对话记录不会删除。
- 网页断开、切换或后端退出只停止 attach 客户端。会话列表、标题和记录绑定由 tmux 保存，后端重启后可恢复。
- 不同网页可以连接不同会话；连接同一会话时，最后连接的网页接管该会话的终端。
- 聊天支持 Markdown、代码块和回复复制。Enter 发送，Shift + Enter 换行，Ctrl / Cmd + Enter 也可发送。
- 工具活动按顺序显示，可展开查看路径、关键词、网页链接或命令。支持开始 / 完成 / 失败状态；活动按原生 CLI 写入记录的时机更新，不是逐 token 流式输出。
- 「终端与审批」处理 CLI 的登录、信任、审批和选项菜单，支持方向键、Tab、Enter、Esc 和 Ctrl C，可复制当前会话的 tmux 接管命令。

服务仅监听 `127.0.0.1`，保持同源校验；允许通过其他网页的链接打开首页。当前支持同一工作目录内的多 agent、多会话，尚未实现远程访问、用户登录或多项目管理。

## 聊天适配

Codex 会话通过 pane 进程打开的 JSONL 绑定实际记录，避免同目录并行会话串线。已验证 CLI 0.162.1，兼容可见消息及 `CommandExecution`、网页 `Extension`、`McpToolCall`、`FileChange` 等活动。

Claude Code 使用独立 `--session-id` 绑定自身项目记录，提取用户 / assistant 文字和可见工具活动，兼容 `CLAUDE_CONFIG_DIR`。启动参数依据 [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)。两种解析器均不展示开发者上下文、内部推理、工具原始结果或读取文件的正文。

Claude Code 的命令和记录格式可能随版本变化。解析异常时仍可通过完整终端操作；自定义 CLI 需要新增记录适配器才可启用聊天模式。

## 构建独立程序

```bash
cargo build --release
./target/release/agent-web-tmux
```

网页和静态依赖随二进制内嵌，运行时无需 Node.js、npm 或外部资源目录；需要 tmux 和对应 agent。macOS 的 Codex 记录绑定使用系统 `ps` / `lsof`，Linux 使用 `ps` / `/proc`。

```text
浏览器 ⇄ WebSocket ⇄ Rust / Axum / PTY ⇄ tmux attach ⇄ 默认 shell ⇄ coding agent
                    HTTP ⇄ 各 agent 的原生记录适配器
```

- `src/agent.rs` / `src/launcher.rs`：agent 配置、参数及 shell 命令选择。
- `src/web.rs` / `src/tmux.rs` / `src/terminal.rs`：会话、HTTP、WebSocket、PTY 和输入。
- `src/history.rs` / `src/claude_history.rs` / `src/session_history.rs`：消息、活动和记录绑定。
- `public/`：网页和随源码提供的静态资源，附有第三方许可证。

新会话使用 `agent-web-<UUID>` 名称。保留原工作目录对应的 tmux socket，并兼容旧的 `codex-web` / `codex-web-<UUID>` 会话。`GET /api/sessions` 列出会话和 agent；`POST /api/sessions` 接收 `{"agent":"claude","cols":90,"rows":25}`。状态、屏幕、聊天、关闭接口通过 `?session=<name>` 选择会话；WebSocket 的 `init` 带 `session`。省略选择的旧接口保留给原 Codex 会话。

## 验证

```bash
cargo fmt --check
npm run check
npm test
npm run test:browser
```

Rust 测试使用隔离的真实 tmux 会话，验证 shell alias / 函数、`cc` / `cx` 回退、参数保留、输入、会话隔离、关闭和后端重启恢复。浏览器测试用模拟的原生格式记录验证 Codex / Claude Code 聊天、自定义 CLI 终端、活动状态、切换、刷新及手机布局，不调用模型。

浏览器测试需要 Node.js 和 Chrome，也可用 `PLAYWRIGHT_EXECUTABLE_PATH` 指定 Chromium。测试截图、本地记录、依赖和构建目录均不提交。
