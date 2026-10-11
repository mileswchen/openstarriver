# starriver-tui

直连 starriver-host 的 Coding Agent 终端 TUI（单任务串行）。

## 运行

```bash
# 默认配置（本地 ollama localhost:11434）
cargo run -p starriver-tui

# 指定配置
cargo run -p starriver-tui -- --config /path/to/config.toml
```

## 命令

| 命令 | 动作 |
| --- | --- |
| `/help` | 帮助 |
| `/new` | 开新会话 |
| `/sessions`（别名 `/resume` `/continue`） | 列出会话并选择续聊 |
| `/projects` | 列出项目 |
| `/project [id\|name]` | 查看/切换当前项目 |
| `/model [provider] [model]` | 切换模型（无参进入两级选择器） |
| `/plan` `/build` `/auto` | 切换 agent 模式（也可 Tab 循环） |
| `/open <path>` | 切换工作目录/项目（全新会话） |
| `/compact` | 压缩当前会话上下文 |
| `/diff` | 当前项目工作区 git diff |
| `/status` | 当前状态（模式/Agent/模型/项目/会话） |
| `/usage` | 当前会话 token 用量与消息数 |
| `/init` | 为当前项目生成 AGENTS.md |
| `/export` | 导出当前会话为 Markdown |
| `/lang [zh\|en]` | 切换界面语言（无参则切换） |
| `/exit` | 退出 |

## 快捷键

- Idle：Enter 提交；Ctrl+C / Ctrl+D 退出；↑↓/PgUp/PgDn 滚动输出；Tab 循环 agent 模式。
- Running：Ctrl+C 取消当前任务。
- 选择列表：↑↓/j/k 移动，Enter 确认，Esc 返回。
- 审批：y 允许，n 拒绝，m 记住选择，Esc 稍后，Ctrl+C 取消任务。

## 状态机

Idle → Running → Idle；运行中锁定输入。事件流：thinking / text / tool / compaction /
approval / completed / failed / cancelled。

## 测试

```bash
# 单元 + 集成测试（不需要模型/网络）
cargo test -p starriver-tui

# 真实 host 冒烟（需要本地临时 DB；默认忽略）
cargo test -p starriver-tui --test real_host_smoke -- --ignored
```
