# Starriver 插件平台架构设计（v2 提案）

> 输入：主流 Agent 插件生态对标（ChatGPT Apps SDK / Claude Agent Skills / MCP / Coze·Dify）。
> 结论先行：主流已收敛到 **MCP（能力）+ Agent Skills（方法论）+ iframe 沙箱桥（UI）**；
> Starriver 三层都有同构实现，**最大差距在执行模型**——插件 UI 只有 `session.execute`
> 一条路，每次调用派生/占用一个宿主会话。本设计以「**会话内工具调用**」为核心补齐执行
> 模型，其余层做标准化对齐，不推倒重来。

---

## 0. 设计原则

1. **声明为主，错误为真**：插件的一切能力（工具、权限、UI、后台运行）必须在 manifest
   中显式声明；越权调用大声失败（现有桥的 `FORBIDDEN` 语义），不做静默降级。
2. **会话是用户的，不是插件的**：插件默认在**当前会话内**或**无会话**执行；只有显式
   声明的后台任务才允许产生独立会话，且必须带来源标记、在专属分组可见——隐藏是投影
   问题，不是持久化问题，审计轨迹永远保留。
3. **复用现有缝，不造第二套运行时**：内核已由 cordis-rslite 组合（services/event bus/
   inject），用户插件不进入进程内；工具挂 `ToolRuntime` 缝，MCP 走现有 bridge，UI 走
   现有 postMessage 桥，卡片走现有 conversation-node 注册表。
4. **标准优先**：SKILL.md 对齐 Agent Skills 开放标准（agentskills.io）；数据能力对齐
   MCP；UI 桥对齐 Apps SDK 的 `window.openai` 语义（私有协议，但概念一一对应，降低
   插件作者迁移成本）。

---

## 1. 现状盘点（代码锚点）

| 层 | 现状 | 锚点 | 差距 |
|---|---|---|---|
| 内核组合 | cordis-rslite：Context/EventBus/Inject/EffectHandle，内核本身即插件组合 | `crates/cordis-rslite`、`docs/tutorial/01-first-plugin.md` | 无（这是内部缝，不是用户插件系统） |
| 用户插件安装 | PLUGIN.md + SKILL.md + `ui/`，git/本地目录安装，`plugins.yaml` 登记 | `crates/starriver-host/src/plugin.rs` | ① 只拷贝 SKILL.md **单文件**，scripts/references 丢失（`plugin.rs:246` 附近 `fs::copy(f, target.join("SKILL.md"))`）；② git 浅克隆无版本锁定；③ 无 MCP/工具声明 |
| 技能 | SkillManager 渐进披露（skill_list/skill_load）、slash 命令分发 | `crates/starriver-skills`、`runtime.rs` execute 内 `resolve_by_command` | 与 Agent Skills 基本同构，保持 |
| 工具缝 | `ToolRuntime::register(ToolDefinition)`：executor、审批标记、并发分类、**presentation meta**（纯函数投影 → `tool/result.meta`，持久化可回放，不进模型） | `crates/starriver-kernel/src/tools.rs:74-140` | 缝完整，但**没有协议层的直接调用入口** |
| MCP | **客户端** bridge：外部 MCP server → ToolDefinition，stdio/HTTP/OAuth/监督重连 | `crates/starriver-mcp-bridge/src/{adapter,config,transport}.rs` | 插件无法在 manifest 里声明自带 MCP server |
| 执行 | 仅 `session.execute`（新建/续用会话，单输入槽 SESSION_BUSY）；`session.waitTurn` 供 headless；消息队列 `queue_add/queue_flush_now` 已存在 | `runtime.rs:1370`、`docs/headless.md`、`agents.rs:846-985` | **无 `tools.call`**（协议只有 `tools.list`）；插件 UI 拿不到「当前会话」上下文 |
| 会话来源 | `session_projcache.origin` 列 + `stamp_session_origin`，定时任务已用 `"scheduled"` 做侧栏分组（WI-152） | `host_store.rs:521`、`admin4.rs:623` | 机制现成，插件来源未接入 |
| UI 桥 | sandbox iframe（仅 `allow-scripts`）+ postMessage `invoke`，manifest 权限白名单过滤，流式订阅转发，声明式 conversation node（转义模板）、主题、`shell:replace` | `desktop/electron/renderer/contrib/pluginHost.tsx`、`conversationNodes.tsx` | ① 无状态托管——且 **`allow-scripts` 无 `allow-same-origin` 的 iframe 是 opaque origin，localStorage 根本不可用**，插件自管状态在当前沙箱下是伪命题；② 卡片只有转义模板，无 inline widget；③ 无 `callTool`/`sendFollowUpMessage` 语义 |

**根因确认**：插件 UI 的每一次数据操作（如 starpaper 搜论文）都被迫走
`session.execute` → 派生宿主会话 → Chat 侧栏增长。这不是插件侧写法问题，是宿主
缺少「无会话工具调用」通道。插件侧复用会话只是缓解（且会撞 SESSION_BUSY 单输入槽）。

---

## 2. 目标架构：一个安装单元，三个贡献面，三条执行通道

```
 PLUGIN.md (manifest v2)
 ├── capabilities: MCP servers / HTTP tools     ──→ ToolRuntime 缝（命名空间化）
 ├── skills: 完整技能目录（Agent Skills 兼容）    ──→ SkillManager（渐进披露）
 └── ui: iframe + bridge v2 + inline widgets    ──→ 现有桥 + conversation-node 注册表

 执行通道（插件 UI/工具按需选择，宿主强制路由）：
 ① tools.call        —— 无会话、无 LLM、无 turn；数据操作的默认通道
 ② 当前会话 follow-up —— 需要 LLM 且用户可见；走消息队列，不新建会话
 ③ 后台 origin 会话   —— 需要 LLM 但不该污染聊天上下文；显式声明，专属分组可见
```

### 2.1 能力面：MCP-first，工具即会话内能力

- **manifest 声明 MCP server**（stdio command 或 HTTP url），安装时宿主把它注册进
  现有 `McpConfig.mcp_servers`，key 为 `plugin:<name>`，由现有 bridge 监督生命周期
  （启动/重连/卸载即断连）。工具以 `mcp__<plugin>__<tool>` 命名进入 ToolRuntime——
  **当前所有会话的模型立即可见**，与 Claude Code 的插件 MCP 语义一致。
- **可选 HTTP 工具声明**（OpenAPI-lite：url/method/params schema），覆盖 Coze 式
  「一个 REST 接口就是一个工具」的轻量插件：宿主合成 ToolDefinition（同一缝），
  作者不必自己跑 MCP server。两条路产物同构，审批/沙箱/超时策略统一。
- **技能内 scripts 不是第二套工具系统**：保持 Agent Skills 语义——模型读 SKILL.md
  后用 bash 调脚本。确定性数据接口才升级为工具。

### 2.2 方法论面：Agent Skills 完整兼容

- 安装时拷贝**整个技能目录**（SKILL.md + scripts/ + references/ + assets/），修复
  当前单文件拷贝；目录名保持 `<plugin>-<skill>` 前缀防冲突。
- frontmatter 保持 `name/description` 必填；预留 `allowed-tools`、`license` 等
  Agent Skills 扩展字段的透传（解析不认识的字段不报错，原样保留）。
- 达成：**同一技能目录不改一行即可被 Claude Code 等兼容客户端加载**；插件的 MCP
  server 声明同时就是标准 MCP 配置，可被 Cursor/Copilot/ChatGPT 复用。

### 2.3 展示面：桥 v2（`window.starriver`）

保持 iframe 沙箱与权限白名单不变，宿主向 frame 注入一个薄 SDK（宿主 served 的
`bridge.js`，或 manifest 声明引入），把裸 postMessage 包装成 Apps-SDK 同构接口：

| SDK 接口 | 语义 | 底层 | 对标 |
|---|---|---|---|
| `callTool(name, args)` | 会话内/无会话工具调用 | 新协议方法 `tools.call` | `window.openai.callTool` |
| `getWidgetState()` / `setWidgetState(s)` | 宿主托管状态，跨轮/跨重启 | 新协议 `widgetState.get/set`，host_store SQLite | `setWidgetState` + `widgetSessionId` |
| `sendFollowUpMessage(text)` | 往**当前聊天会话**发消息 | `queue_add`（运行中排队）或 `session.execute(sessionId)`（空闲时），见 §3.2 | `sendFollowUpMessage` |
| `context` | `{ sessionId?, toolInput?, toolOutput?, theme, locale }` | 桥在 frame 加载/事件时下发 | `toolInput/toolOutput/theme` |
| `openExternal(url)` | 经宿主校验后系统浏览器打开 | 新协议 `shell.openExternal`（http/https 白名单） | `openExternal` |
| `registerNodes(...)` | 声明式卡片（保留）＋ widget 卡片（新增） | 现有 conversation-node 注册表扩展 | `_meta.ui.resourceUri` |

裸 `invoke` 通道保留（白名单过滤不变），SDK 只是其上的语义层——老插件不破坏。

**inline widget（消息旁的小组件）**：复用**已经存在**的 presentation-meta 通道——
工具定义返回 `meta.ui = { resource: "ui/widgets/paper-card.html", data: … }`，
`tool/result.meta` 持久化并回放（`tools.rs` 的 `ToolPresentationMeta` 契约：纯函数、
软失败），渲染端 conversation-node 匹配到 `meta.ui` 时渲染 sandbox iframe，桥把
`toolInput/toolOutput` 注入 frame context。**历史回放时 widget 原样重建**，这是
localStorage 方案永远做不到的。模板节点（转义字符串）保留为轻量档，widget 是重档，
同一注册表按 priority 竞争。

---

## 3. 执行模型（核心变更）

### 3.1 通道①：`tools.call` —— 新增协议方法

```jsonc
// 请求
{ "method": "tools.call",
  "params": {
    "name": "mcp__starpaper__search_papers",
    "arguments": { "query": "agent skills", "limit": 10 },
    "sessionId": "s-123",        // 可选：提供 cwd/沙箱上下文；缺省用宿主默认工作区
    "timeoutMs": 30000
  } }
// 结果：与 tool/result 同构
{ "ok": true, "value": …, "content": […], "meta": … }
```

实现要点：
- 走 **ToolRuntime 完整管线**（pre-execute 审批触发、只读沙箱拒绝、超时、
  finalize_content），不是旁路。`requires_approval` 的工具照样弹宿主审批卡——
  桥已能转发流式事件，审批 UI 由 shell 承担，插件 frame 不自绘审批。
- **不产生 turn、不调 LLM、不创建会话**；`sessionId` 只借上下文，不占用输入槽
  （与正在跑的 turn 并行不冲突；工具自身的并发分类照常生效）。
- 权限：新增 `tool:call`。插件默认只能调**自己命名空间**的工具
  （`mcp__<self>__*`）；跨插件/宿主工具需 manifest 显式声明
  `toolAllowlist: ["mcp__other__x", "web_search"]`。越权 → `FORBIDDEN`，大声失败。
- headless 驱动同样受益：脚本/CI 可以 `tools.call` 直取数据，不再为一次检索
  起一个完整 agent turn（`docs/headless.md` 的 execute+waitTurn 保留给真 agent 任务）。

### 3.2 通道②：当前会话 follow-up

插件需要 LLM 推理且结果应出现在对话里（如「帮我写这篇笔记」）时：
- 桥把**当前聊天会话 id** 注入 frame context（Chat 页签挂载的 widget 才有；
  独立 Tab 的插件没有当前会话，只能走通道③或要求用户选择会话——显式，不猜）。
- `sendFollowUpMessage` 的空闲/运行中分流：会话空闲 → `session.execute(sessionId)`
  续用；运行中 → `queue_add`（现有 FIFO/Send-Now 队列，`agents.rs:889`），由 agent
  driver 在同一 execute 内认领——**绕开 SESSION_BUSY，且侧栏零增长**。
- 该通道需要 manifest 声明 `session:execute`（现有权限常量），UI 上表现为用户
  可见的一条消息，语义与用户亲手输入一致。

### 3.3 通道③：后台 origin 会话

插件的批处理/定时任务（如「每天追踪作者新论文」）不该混进聊天上下文：
- manifest 声明 `background: true` + 用途描述；安装时用户确认一次。
- 执行仍走 `session.execute`（完整持久化、可审计、可回放），但宿主
  `stamp_session_origin(sid, "plugin:<name>")`——**复用 WI-152 的 origin 机制**，
  侧栏按 origin 分组：Chat 列表过滤 `origin LIKE 'plugin:%'`，插件自己的 Tab 内
  展示「运行记录」（`sessions.list` 加 origin 过滤参数）。
- 与 `archived` 不同：不是藏起来，是**放到正确的抽屉里**。用户随时可点开看完整
  transcript；卸载插件时会话保留（归入通用「后台运行」组），不静默删除。

> **P2 演进（已落地）**：通道③不再让插件裸调 `session.execute`，改为专用
> `pluginTask.run` 原语（见 §7「P2 实施状态」）。插件任务会话**完全退出 Chat
> 表面**（不再进侧栏「插件任务」分组），审计入口收敛为 `pluginTask.list` 与
> `session.list?origin=plugin:<name>` 两条显式路径。P1 的侧栏分组投影被移除
> ——这是一处**已声明的可见性破坏性变更**：会话仍永久保留、可审计，只是不再
> 出现在聊天侧栏。裸 `session.execute`（声明 background 者）仍可用，同样盖章
> `origin=plugin:<name>` 并从 Chat 表面隐藏，作为兼容路径保留。

### 3.4 路由决策表（写进插件开发文档）

| 插件要做的事 | 通道 | 会话影响 |
|---|---|---|
| 搜论文/查数据/调 API（确定性） | ① `tools.call` | 无 |
| 读/写插件运行态 | `widgetState.get/set` | 无 |
| 让 agent 在当前对话里继续干活 | ② follow-up（队列） | 当前会话 +1 轮 |
| 定时/批量 LLM 任务 | ③ `pluginTask.run`（P2；裸 execute 为兼容路径） | 内部任务会话，Chat 表面零增长，审计走 `pluginTask.list` |
| ~~任何场景~~ | ~~无 sessionId 的裸 execute~~ | **禁止**：桥对未声明 background 的插件屏蔽无 sessionId 的 execute |

最后一行是止血阀：桥层（`pluginHost.tsx` 的 invoke 过滤处）对 `session.execute`
追加参数校验——无 `sessionId` 且插件未声明 `background` → 拒绝并提示改用
`callTool`/follow-up。**存量插件的痛点在桥层立即止血，不等插件改造。**

---

## 4. 状态托管：`widgetState`

- 存储：host_store（SQLite）新表 `widget_state(plugin, surface, session_id, key, value_json, updated_at)`；
  `surface` = tab id 或 inline-widget 的 tool call id；`session_id` 可空（全局态）。
- 作用域即安全边界：插件只能读写自己 `(plugin, *)` 命名空间，无需额外权限常量。
- 配额：单插件总量上限（建议 1 MB）+ 单值上限（64 KB），超限报错不截断。
- 语义对齐 Apps SDK：inline widget 的状态键含 sessionId → **同一会话重开，widget
  恢复原状**；跨轮由宿主保证，插件不再自管持久化（当前 opaque-origin iframe 里
  localStorage 本就不可用，此接口是补洞不是锦上添花）。

---

## 5. Manifest v2（PLUGIN.md frontmatter + 约定目录）

```yaml
---
name: starpaper
version: 0.2.0
description: 论文检索/问答/综述工作台
capabilities:
  mcp:                          # 可选：自带 MCP server（标准 McpServerConfig 子集）
    - transport: stdio
      command: uvx
      args: [starpaper-mcp]
  httpTools:                    # 可选：OpenAPI-lite 声明（与 mcp 二选一或并存）
    - name: search_papers
      url: https://api.openalex.org/works
      method: GET
      parameters: { … JSON Schema … }
skills: auto                    # 扫描目录内全部 SKILL.md（含完整子目录）
ui:
  entry: ui/index.html          # 现有 ui/manifest.json 内容并入此处，兼容期两者都认
  tabs: [{ id: workbench, title: 论文, titles: { zh: 论文, en: Papers } }]
  permissions: [session:read, tool:call]
  toolAllowlist: []             # 跨命名空间工具白名单，默认空
  background: false             # 通道③开关，安装时需用户确认
---
```

目录布局（= Agent Skills 超集，技能子目录可单独拷去 Claude Code 用）：

```
starpaper/
├── PLUGIN.md
├── skills/
│   ├── paper-search/         # SKILL.md + scripts/ + references/ 完整拷贝
│   └── …
├── mcp/                      # 可选：server 实现源码
└── ui/                       # index.html + widgets/*.html + theme.css + icon.svg
```

---

## 6. 生命周期与分发

- **安装**：现有 `install_plugin` 扩展——① 拷贝完整技能目录；② 解析 manifest v2
  并注册 MCP server / HTTP 工具；③ git 源记录 **commit SHA 锁定**（当前浅克隆
  无锁定，供应链风险），`plugins.yaml` 存 `resolved_sha`；`plugins.update` 显式
  拉新并展示 diff 摘要后重锁。
- **卸载**：现有清理 + 断连 MCP server + 注销 HTTP 工具 + 清 widget_state +
  conversation-node 按 owner 前缀清理（已有）。origin 会话保留。
- **分发**：近期不做中心商店。加 `marketplace` 概念（对齐 Claude Code）：一个 git
  仓库放 `marketplace.json`（插件名 → git url + 描述），`plugins.marketplaceAdd`
  后可浏览/一键安装——复用现有 git 安装路径，成本极低。
- **互操作出口（展望）**：starriverd 增加 MCP **server** 门面，把宿主工具/技能暴露
  给外部客户端（Cursor/Claude Code 反向使用 Starriver 能力）。与本文正交，单列 backlog。

---

## 7. 分期实施与验收

| 期 | 内容 | 验收标准 |
|---|---|---|
| **P1 执行模型止血** | `tools.call` 协议方法 + 桥 `callTool`；桥对无 sessionId 的 execute 按 §3.4 拦截；origin=`plugin:<name>`  stamping + 侧栏过滤 + `sessions.list` origin 参数 | starpaper 检索走 `tools.call`，Chat 侧栏**零新增会话**；后台任务出现在插件分组；越权调用返回 FORBIDDEN |
| **P2 无会话任务原语** | `pluginTask.*` 六方法（run/wait/messages/cancel/approve/list）+ runId 所有权 + manifest 权限上限 + 页内审批中继；插件任务会话完全退出 Chat 表面 | starpaper 深度任务走 `pluginTask.run`，Chat 侧栏零新增、无插件分组；跨插件访问 FORBIDDEN；超上限请求被桥拒绝；高危操作页内审批 |
| **P2.5 状态与 follow-up**（原 P2，顺延） | `widgetState.get/set` + 配额；frame context 注入 sessionId；`sendFollowUpMessage` 空闲/队列分流 | 关掉重开会话 widget 状态恢复；运行中 follow-up 入队不报 SESSION_BUSY |
| **P3 标准化** | 完整技能目录拷贝；manifest v2（MCP/httpTools 声明 + SHA 锁定）；`bridge.js` SDK | 技能目录原样放进 Claude Code 可用；插件 MCP 工具在当前会话模型工具列表可见 |
| **P4 展示与分发** | presentation-meta inline widget；marketplace.json；`plugins.update` | 工具结果旁渲染 sandbox widget 且历史回放一致；一条命令加市场装插件 |

P1/P2 独立可发布，直接解决用户痛点；P2.5–P4 互不阻塞。

### P1 实施状态（已落地，feat/tool-env 分支）

- **① tools.call**：`crates/starriver-host/src/tool_call.rs` + catalog `tools.call`（perm `tool:call`）。无会话、无 turn、无 LLM，走完整 ToolRuntime 管线；超时默认 30s、上限 600s。**偏差**：需审批的工具在无会话上下文时 fail-closed 返回 `APPROVAL_REQUIRED`（DangerFullAccess 除外）——设计承诺的审批卡片留到 P2，不做静默放行。
- **② 桥拦截**：`pluginHost.tsx` 导出 `sessionSpawnDenied`/`pluginCallParams`。裸 `session.execute`（无 sessionId）一律 FORBIDDEN，除非 manifest `ui.background: true`（`PluginUiEntry.background` 全链路透传：install→admin→catalog→前端）；错误信息给出三条出路（tools.call / 带 sessionId / 声明 background）。桥对每次 `session.execute` 强制覆写 `origin=plugin:<name>`，frame 无法伪造身份。callTool 便捷封装随 P3 bridge.js SDK 提供，P1 经通用 invoke 调 `tools.call`。
- **③ origin stamping**：`execute()` 第 9 参 `origin`，仅接受 `plugin:<name>`（name 1–128 字符 `[a-zA-Z0-9._-]`）；**创建时一次性盖章**——新鲜度锚点在本轮自身 stamp 之前捕获（projcache 行或会话日志存在即非新鲜），续聊永不改标、永不把用户 Chat 改判为插件任务。`session.list` 增加 origin 精确过滤参数；侧栏 Tasks 下新增「插件任务 / Plugin runs」分组（纯投影，不隐藏不删除）。
- **验证**：host lib 838/839（余 1 为 bash 沙箱机制环境测试）、protocol-server 48/48、tui 134/134、acp 1/1、前端 690/697（余 7 为存量 provider-probe mock 问题，stash 基线实验证实与本次改动无关）、tsc 干净。

### P2 实施状态（已落地，feat/tool-env 分支）：无会话插件任务执行原语 `pluginTask.*`

**「无会话」的准确含义**：指**聊天表面无会话**（chat-surface sessionless），不是存储层无会话。
插件深度任务仍复用宿主内部任务会话（完整持久化、可回放），但这些会话带
`origin=plugin:<name>` 戳，**从所有 Chat 表面排除**，只经显式审计路径可见。存储层
真正无会话（任务不落会话日志）是 P3+ 议题，本期不做。

#### 原语契约（6 方法，`protocol_catalog.rs`）

| 方法 | 权限 | 入参 | 返回 | 备注 |
|---|---|---|---|---|
| `pluginTask.run` | `task:run` | `plugin,task,runId?,permissionPreset?,agent?,agentPreset?,clientRequestId?` | `{runId,subscription}` | **流式**：返回 run 专属事件订阅；`runId` 续用即追问/续任务 |
| `pluginTask.wait` | `task:read` | `plugin,runId,timeoutMs?` | `{ended,reason,finalAssistant}` | 默认 300s、上限 3600s；reason=`completed`/`timeout`/`session_not_running` |
| `pluginTask.messages` | `task:read` | `plugin,runId,limit?,before?` | `{messages,total,hasMore}` | 恒分页 |
| `pluginTask.cancel` | `task:run` | `plugin,runId` | `{cancelled}` | |
| `pluginTask.approve` | `task:run` | `plugin,runId,allowed,remember?` | `{}` 或 `APPROVAL_NOT_PENDING` | 应答宿主发起的审批请求 |
| `pluginTask.list` | `task:read` | `plugin` | 本插件的 run 列表 | 审计入口 |

`plugin` 身份由桥强制覆写注入（见下），frame 无法伪造。`run` 的流式订阅经
`HostRuntime::spawn_event_relay(receiver, emit)` 中继（api-macro 流式模板统一调用），
事件信封 `{v,subscription,seq,global_seq?,type,payload}`，UiEvent JSON 在 `payload`。

#### 所有权模型

- **runId = 内部任务会话 id**。`plugin_task_run` 在调 `execute()` **之前同步盖章**
  `stamp_session_origin(sid, "plugin:<name>")`（UPSERT）——所有权即刻可校验、跨重启持久。
- 桥（`pluginHost.tsx` `pluginCallParams`）对**每个** `pluginTask.*` 调用强制覆写
  `plugin=<manifest name>`；宿主 `plugin_task_owned` 校验 projcache origin ==
  `plugin:<caller>`：他人 run → `FORBIDDEN`，不存在 → `NOT_FOUND`。
- 插件名约束 `[a-zA-Z0-9._-]{1,128}`。`pluginTask.list` 只返回本插件 run。

#### 可见性规则

- `session.list` **无 origin 过滤时默认排除** `plugin:%`（只返回用户 Chat）；
  `listArchived`、`sessions.running` 同样过滤。精确 `origin=plugin:<name>` 过滤保留为审计路径。
- 侧栏 P1「插件任务 / Plugin runs」分组**已移除**（`app.tsx`）——已声明的破坏性变更：
  插件任务不再出现在聊天侧栏，审计走 `pluginTask.list` / `session.list?origin=…`。
- 兼容路径：声明 background 的插件仍可裸调 `session.execute`，桥同样强制盖章
  `origin=plugin:<name>` 并从 Chat 表面隐藏。

#### 权限上限治理（manifest `maxPermissionPreset`）

- manifest 声明 `ui.maxPermissionPreset`（camelCase；serde rename + alias
  `max_permission_preset`），是插件 UI **能请求的最高权限档**——对用户的自治承诺。
- 档位序：`manual(0) < accept-edits(1) < auto(2) < danger-full-access(3)`。
- 桥 `presetCeilingDenied` **fail-closed**：未声明 → 上限 `manual`；请求档 > 上限 → 拒绝；
  未知档名 → 拒绝；上限声明畸形 → 按 `manual`。省略请求档 → `manual` → 放行。
- 门控覆盖 `pluginTask.run` / `session.execute`（`permissionPreset` 参数）**以及
  `session.setPermissionPreset`（`preset` 参数）——后者是 P1 遗留的提权旁路，本期闭合**。
- starpaper 声明 `maxPermissionPreset: "auto"`：检索类低风险命令自动放行，命中高危
  规则升级人工审批；权限白名单收敛为 `["task:run","task:read"]`（不再需要 session:*）。

#### 审批 UX 与已知缺口

- **审批权威始终在宿主**：风险分级 + 上限强制 + `ApprovalBridge` 仲裁都由宿主承担。
  插件页可把宿主发起的 `approvalRequested`（`{sessionId,requestId,toolName,callId,reason,params}`）
  渲染成页内卡片，并经 `pluginTask.approve` **中继用户决定**——这是呈现中继，不是自审批：
  无待决请求时返回 `APPROVAL_NOT_PENDING`，插件无法给自己授权，无应答则宿主 fail-closed 阻塞。
- 事件经 **run 专属订阅**转发（桥按 subscription 隔离，`getClient().subscribe`），
  **不是** xhs-seeding 2026-11-13 教训里的 last-wins 全局广播——后者才会静默饿死。
- **已知缺口（如实记录）**：桥在 iframe 卸载时清理订阅。宿主切走再切回会重建插件
  iframe，旧 run 订阅失效，**等待中的审批卡片无法恢复**（协议层暂无「查询待决审批」
  方法，`ApprovalBridge` 的 pending map 未对外暴露）。缓解：starpaper `monitorTurn`
  做停滞检测——无审批卡片且 30s 无新消息时，状态栏如实提示「可能等待审批或长思考
  （可停止重试）」，代替静默挂起。彻底修复（待决审批查询 / 重订阅）留待后续。

#### 生命周期

- 内部任务会话**永久保留，不自动删除、不自动归档**；卸载插件时会话保留，不静默删除。
- 复用 runId 即续任务；「新开会话」按钮才清空 runId。运行态持久化供 iframe 重建后重新挂载。

#### 验证

host lib 842/842、protocol-server 48/49 单线程（余 1 `multi_client_single_input_slot_and_event_broadcast`
为存量环境问题，clean tree git stash 实验证实与本次无关；并行跑另有 HOME-pin 竞争 flake）、
api-macro 18/18、tui/acp 通过、前端 698/705（余 7 为存量 `__provider_probe_transport` mock 问题）、
tsc 干净。新增 `plugin_task_primitive_ownership_and_visibility`（9 步：同步盖章断言、
跨插件 FORBIDDEN×5、用户 Chat FORBIDDEN、APPROVAL_NOT_PENDING、INVALID_PARAMS）+
桥接 branding/ceiling 8 测试 + manifest ceiling 解析测试。

## 8. 明确不做

- 不给用户插件开进程内运行时（cordis-rslite 保持内部缝）。
- 不做插件**自授权**——审批决定永远由宿主仲裁（风险分级 + 权限上限 + ApprovalBridge）。
  插件页可渲染宿主发起的审批请求并经 `pluginTask.approve` 中继用户决定（P2，呈现中继
  而非自审批）；无应答宿主 fail-closed 阻塞，插件无法给自己提权。
- 不静默删除任何会话——插件任务会话永久保留、可审计（`pluginTask.list` /
  `session.list?origin=…`）。P2 把它们从 Chat 表面排除（取代 P1 的侧栏分组投影），
  这是已声明的可见性变更，不是静默隐藏：审计入口始终显式存在。
- 不放宽 iframe 沙箱（不加 `allow-same-origin`）来「修」localStorage——用宿主托管替代。
- 不在 P1–P4 内做中心商店/计费/签名体系——git + SHA 锁定先行。