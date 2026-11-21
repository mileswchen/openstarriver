# DSH 式循环守卫落地设计（Starriver）

状态：设计评审稿（turn-19）。配套问题诊断见 `degenerate-loop-false-positive.md`（§1–§7）。
本文把 DSH `repeat-tool-reminder` 的方案映射到 Starriver **真实代码接缝**，给出可落地设计。

## 0. 一句话结论

Starriver **已经有 DSH 用的同款接缝**（advisory 注入缝 + 动作身份键 + per-agent/per-turn 守卫 +
资源预算兜底），所以这不是「新造一个插件系统」，而是：
**① 中和散文检测器的硬中止（误报根）→ ② 复用既有缝加一个 advisory 动作身份提醒 →
③ 保留资源预算/失败熔断作正交兜底 → ④ 诚实暴露任何回合终止。**

## 1. 设计原则（DSH 精髓 → Starriver 映射）

| DSH 原则 | Starriver 落地 |
|---|---|
| 不监视散文，只监视**动作身份** | 移除 `DegenerateLoopMonitor` 的 TextDelta 硬中止；新守卫键=`call_signature(工具名,规范化参数)` |
| **advisory**，从不 block/改写/中止 | 提醒经既有 `accept_context`→inbox `NextStep` 注入下一轮；调用照常执行 |
| 精确规范化键（深排序键，忽略属性顺序） | 升级 `call_signature` 为深排序键+JSON 序列化（当前仅去空白，有属性顺序漏检 gap） |
| 每 agent 独立链，用户插话重置 | 守卫已是 per-agent 字段 + per-turn 重置（Starriver 一个 turn=一条用户 prompt，语义对齐） |
| 阈值 3/5/8 温和→详细，参数预览≤500 | 同款阈值/文案/预览上限，走 `AgentOptions` 配置缝 |
| 「plugins, not loop changes」叶子化 | 新叶子模块 `repeat_tool_reminder.rs`，复用既有缝，不改动主循环脊柱逻辑 |
| 资源预算作正交兜底（Codex 式） | 保留 `TURN_TOOL_CALL_BUDGET=666` + token 预算 |
| 配置 fail-loud | 阈值空/非整数/<2/重复 → 构建期报错，不静默回退 |

## 2. 既有接缝盘点（实证，证明无需新原语）

| 能力 | Starriver 现状 | 位置 |
|---|---|---|
| **advisory 注入缝** | `accept_context(Message)` → `inbox.splice(InboxTarget::NextStep,…)`，喂下一轮请求且经 `agent/inbox/spliced` 落日志 | `agent.rs:1709-1715,1726`；`tool_calls.rs:343-344` 消费 `result.additional_contexts()` |
| **plugin/notice provenance** | `MessageSource::Plugin{plugin,form:Option<ContextForm>}` + `ContextForm::Notice{summary}` | `kernel/llm.rs:219,237` |
| **现成 notice 范式** | `append_anti_loop_notice` 已用 `Plugin{plugin:"harness:anti-loop"}`+`append_snapshot_message` | `agent.rs:268-281` |
| **动作身份键** | `call_signature(name,args)`=去空白 JSON 串（⚠️ 未深排序键） | `tool_calls.rs:602` |
| **既有重复守卫** | `TurnRepeatGuard`：只数**连续失败**（成功清零），`REPEAT_FAILURE_LIMIT=2`→第3次相同失败**硬终止回合**；transport 失败排除(WI-202) | `tool_calls.rs:490-600`；调用点 `agent.rs:1480-1560` |
| **资源兜底** | `TURN_TOOL_CALL_BUDGET=666`（变参风暴兜底）+ `ReloadStorm` 专用 reload 熔断 | `tool_calls.rs:465,525`；`agent.rs:1474,1577` |
| **per-agent/per-turn 状态** | `self.turn_repeat`/`self.reload_storm` 为 Agent 字段，turn 起始 `reset_keeping_cursor()` | `agent.rs:208,214,987-992` |
| **配置缝** | `AgentOptions.turn_tool_call_budget/max_parallel_tool_calls/…`，从 settings `agent.*` 填充，next-run 生效 | `kernel/agent.rs`（AgentOptions） |
| **散文检测器（误报根）** | `DegenerateLoopMonitor` 喂 TextDelta，trip→anti-loop notice+escalate_retry→Aborted DEGENERATE_LOOP | `agent.rs:1060,1184-1240`；`degenerate_loop.rs` |

> 关键差异：现有 `TurnRepeatGuard` 只数**失败**且**硬终止**；DSH 数**所有相同调用**（含成功）且**只提醒**。
> 新守卫是「数所有相同调用 + advisory」，与现有失败熔断**并存**（前者早提醒给模型自纠机会，后者作最后兜底）。

## 3. 组件设计

### 组件 A：中和散文检测器（治本，误报类整体消失）

**移除 `agent.rs:1184-1240` 的 `degenerate.feed(text)` 硬中止分支**（trip→escalate→Aborted）。
- 推荐**彻底移除散文监视**（DSH/Codex 均不监视散文）：模板枚举不再被任何内容守卫判罚 → 零误报。
- `degenerate_loop.rs` 模块与其 12 个测试随之退役（或保留为纯可观测 metric，**绝不 trip/中止**）。
- `append_anti_loop_notice`（:268）保留为通用 steering 工具，但不再由散文 trip 触发。
- **取舍（须拍板，见 §7）**：纯散文循环（turn-9「正在编写实现…」×200，不调工具）从此不被内容守卫捕获，
  只剩 turn/token 预算兜底。DSH/Codex 都接受此取舍。若不接受 → 需另设语义级进度信号，而非回字符统计老路。

### 组件 B：新 advisory `RepeatToolReminder`（DSH 蓝图移植）

新叶子模块 `crates/starriver-agent-loop/src/repeat_tool_reminder.rs` + Agent 字段 `repeat_reminder: Mutex<RepeatToolReminder>`。

**B1. 规范化键（升级 `call_signature`）**
```
canonical(args) = JSON.stringify(deep_key_sort(args))   // 深排序键，忽略属性顺序
key = name + '\0' + canonical(args)                      // 检测键始终用完整串
```
- 当前 `call_signature` 仅去空白 → 升级为深排序键（DSH parity），属性顺序不同的同一调用计为相同。
- 保留向后兼容：失败熔断仍可用同一键（升级后更准）。

**B2. 计数语义（DSH 对齐）**
- 状态 `Chain{ key:String, count:u32 }`，per-agent。
- 每个**被跟踪**调用：`key==chain.key → count+=1`；否则 `chain={key,count:1}`。
- **数所有相同调用，不论成功/失败**（DSH 语义；与现有只数失败的熔断互补）。
- **exclude 的工具透明**：既不计数也不重置链（DSH 语义）。include 非空时只跟踪匹配者。
- **重置**：turn 起始重置（=DSH 用户插话重置；Starriver turn=用户 prompt）。子 agent 各自 Agent → 各自链。
- **计数点**：post-execute（DSH 在 post-execute 计数，被 deny 的调用也计数）。
  Starriver 落地：扩展 `TurnRepeatGuard::absorb_results` 的 tail-scan（已 join tool/call→tool/result），
  增一条「连续相同**已执行**调用」计数（含 deny 产生的 result 行），与现有「连续失败」计数并行。
  提醒在下一轮 walk 注入（reminder 到达模型下一 step，时序与 DSH additionalContexts 一致）。

**B3. 阈值与文案（DSH 同款）**
- 默认 `thresholds=[3,5,8]`：第 3 次温和提醒；第 5/8 次详细提醒（点名工具、连续次数、参数预览）。
- 温和文案：「你在用完全相同的参数重复同一工具调用。先分析上次结果：若任务未完成，换方法或换参数，不要重复调用。」
- 详细文案：`Repeated tool call: tool={name} consecutive={count} arguments={preview≤500}` + 「这些重复没有进展。不要再用这组完全相同的参数调用此工具。检查最新结果，换动作/换参数，或证据已足则收尾。」
- `argumentsPreviewChars=500` **只截提醒文本，不截检测键**。
- 仅在**恰好等于**配置阈值时提醒（超过最高阈值后链静默，DSH 语义）。

**B4. 注入（advisory，复用既有缝，绝不 block）**
```rust
// 命中阈值时构造 notice，经既有 accept_context / append_snapshot_message 注入 NextStep
Message {
  role: User,
  content: [Text(reminder)],
  source: Plugin { plugin: "harness:repeat-tool-reminder",
                   form: Some(ContextForm::Notice { summary }) },
}
```
- 调用**照常 dispatch**，不 skip/不改写/不终止。提醒只是下一轮上下文里的一条 plugin notice。
- 落日志（`agent/inbox/spliced`）→ 满足 DSH「model-visible ⟺ logged」规则。
- 复用 `append_anti_loop_notice` 同款 `append_snapshot_message` 路径（compact-managed）。

**B5. 配置 fail-loud**
- 阈值空/非整数/<2/重复 → Agent 构建期报错（DSH 加载即抛错 parity），不静默回退。

### 组件 C：既有硬熔断的去留（决策点）

现有三类硬终止：① `TurnRepeatGuard` 连续3次相同**失败**→终止；② `ReloadStorm` reload 风暴→终止；
③ `TURN_TOOL_CALL_BUDGET=666` 变参风暴→终止。

- **推荐保留 ①②③ 作最后兜底**，理由：它们只在「相同调用反复**失败**」或「资源耗尽」时触发，
  **对合法枚举（成功输出）零误报**——与散文 abort 的误报性质完全不同。
- 新 advisory 提醒在**第3次相同调用（不论成败）**就先给模型自纠机会；若模型仍猛敲相同失败调用，
  失败熔断才终止 → 形成「advisory 早提醒 →（持续）→ 硬兜底」的合理升级。
- **但必须配组件 D**：硬终止的回合要**诚实暴露为 blocker**，不再静默 `Completed`/「完成·44字」。
- 备选（纯 DSH parity）：连失败熔断也降为 advisory，只留资源预算兜底。更激进，放弃「相同失败死循环」的硬保护。见 §7 决策。

### 组件 D：诚实暴露回合终止（turn-12，正交但必需）

- 任何由熔断/预算导致的回合终止，`WaitTurnOutcome`/protocol 映射须给插件 `pluginTask.wait` 一个**失败/blocker 信号**，
  而非 `Completed→reason:"completed"`（现状 `runtime.rs:131,900-910`；`protocol_catalog.rs:1172`）。
- 终止原因（repeat-breaker / budget / reload-storm） surfaced 到 UI 与插件，可恢复。
- 与检测器精度无关，独立有价值；是「无静默降级」原则的直接落实。

### 组件 E：配置缝（AgentOptions + settings）

`kernel/agent.rs` AgentOptions 增字段（`Option<>`，从 settings `agent.repeat_tool_reminder.*` 填充，next-run 生效，
与 `turn_tool_call_budget` 同款语义）：
```
repeat_reminder_enabled: Option<bool>            // 默认 true
repeat_reminder_thresholds: Option<Vec<u32>>     // 默认 [3,5,8]
repeat_reminder_include: Option<Vec<String>>     // 默认 [] = 全部工具
repeat_reminder_exclude: Option<Vec<String>>     // 默认 [] ；建议预置 reload_image/todo_write 等幂等工具
repeat_reminder_args_preview_chars: Option<usize>// 默认 500
```
- 通配 `*` 匹配工具名（DSH include/exclude 语义）；匹配不到已注册工具的模式仍合法。
- 构建期 fail-loud 校验（B5）。

### 组件 F：测试计划

**新增回归（治本验证）**
- §6 六格式模板枚举（table/bibtex/testcase/timeline/API/translation）+ 啰嗦论文卡，作为**散文**流式输出
  → **不中止、不产生误报提醒**（散文不再被监视）。这是误报类整体消失的判据。
- 真循环对照：相同工具+相同参数连续调用 → 第3/5/8次各注入一条 notice；调用仍执行；回合不因此终止。

**advisory 守卫单测**
- 键规范化：属性顺序不同的同一调用计为相同；不同调用重置链。
- exclude 透明（不计数不重置）；include 过滤。
- 新 turn 重置；子 agent 链隔离。
- deny 调用也计数（post-execute）。
- 参数预览≤500 截文本不截键；恰好阈值触发、超最高阈值静默。
- 配置 fail-loud：空/非整数/<2/重复阈值构建期报错。

**既有测试处置**
- `degenerate_loop.rs` 12 测试 + `agent.rs:8546,8600` 散文中止测试 → 随组件 A 退役/改写为「不中止」断言。
- 失败熔断/reload/预算测试（`agent.rs:5709,5854,6122,8644`；`tool_calls.rs:2048`）→ 保留（兜底仍在）。
- 新增组件 D 诚实暴露测试：熔断终止 → `pluginTask.wait` 报失败。

## 4. 状态机（advisory 守卫，与既有熔断的关系）

```
每个被跟踪工具调用（post-execute，含 deny）:
  sig = canonical_key(name, args)
  if exclude(name): pass            # 透明，不动链
  elif sig == chain.key: chain.count += 1
  else: chain = {sig, 1}
  if chain.count ∈ thresholds:
      inject Notice(reminder)  → inbox NextStep   # advisory，调用照常执行
  # 并行：既有 TurnRepeatGuard 数连续失败；≥2 → 硬熔断兜底（组件C/D）
新 turn（用户 prompt）: chain 重置
```
- advisory 链（数所有相同调用）与失败熔断链（数连续失败）**独立并存**，互不干扰。
- 合法幂等轮询（成功重复）：advisory 会在过阈值时提醒（DSH 自陈局限），靠 exclude/thresholds 泄压；
  **失败熔断不会误触发**（成功清零）。

## 5. 落地顺序

1. **组件 A**：移除散文硬中止（治本，误报类消失）+ 退役/改写相关测试。
2. **组件 B**：新 `RepeatToolReminder` 叶子模块 + 升级 `call_signature` 深排序键 + 接入 walk/absorb_results + inbox 注入。
3. **组件 E**：AgentOptions/settings 配置缝 + fail-loud 校验。
4. **组件 C**：确认保留失败/reload/预算兜底（默认），advisory 早提醒在其前。
5. **组件 D**：诚实暴露回合终止（WaitTurnOutcome/protocol/UI/插件）。
6. **组件 F**：回归 + 单测全绿；`cargo test` 守门（加载 cargo-test-discipline）。
7. 文档：PLUGIN.md/技能模板补「结构化输出走工具结果/文件，勿堆散文」通用规范（锦上添花，非前置）。

## 6. 诚实局限

- **纯散文循环**不再被内容守卫捕获（组件 A 取舍），仅 turn/token 预算兜底。DSH/Codex 均接受。
- advisory **仅精确匹配**：近义变体（换标点/同义改写参数）漏检——DSH 明确拒绝模糊匹配（招误报）。
- 合法幂等轮询过阈值仍被提醒（DSH 自陈），靠 exclude/thresholds 泄压。
- 压缩不重置 advisory 链（DSH 同款；Starriver turn 重置已覆盖大多数场景）。
- 若保留失败熔断（组件 C 推荐），它仍会硬终止「相同失败死循环」——但零误报于合法枚举，且组件 D 保证诚实暴露。

## 7. 待决策点

1. **散文检测器**：彻底移除（推荐，DSH/Codex parity）vs 降级为 advisory notice（仍会唠叨合法枚举，不推荐）。
2. **是否接受「放弃纯散文内容守卫、换零误报+预算兜底」取舍**（DSH/Codex 均接受）。
3. **失败熔断去留**：保留作兜底（推荐，零误报+advisory 早提醒）vs 纯 advisory 连失败也只提醒（激进 DSH parity）。
4. **守卫包装**：agent-loop 内叶子模块（推荐，复用既有缝，最小改动）vs 真 host 插件（需 tools/post-execute 插件钩子缝，更 DSH「plugins not loop changes」但需先确认/建钩子）。
5. **exclude 预置**：是否默认排除 reload_image/todo_write 等幂等工具。
6. **组件 D 范围**：是否本轮一并修 WaitTurnOutcome 诚实暴露（推荐，独立有价值）。