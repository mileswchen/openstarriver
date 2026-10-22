// AUTO-GENERATED from protocol.schema.json — do not edit by hand.
// Regenerate with: npm run gen:client

export const PROTOCOL_VERSION = 1;

export interface ProtocolMethodMeta {
  params: unknown;
  result: unknown;
  streaming: boolean;
  permissions: string[];
}

export const PROTOCOL_METHODS: Record<string, ProtocolMethodMeta> = {
  "session.list": { params: {"type":"object","properties":{"workspaceId":{"type":"string"}},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["session:read"] },
  "session.messages": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"limit":{"type":"integer"},"before":{"type":"integer"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.archiveMessages": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.fork": { params: {"type":"object","properties":{"sourceId":{"type":"string"},"beforeMsgId":{"type":"integer"},"task":{"type":"string"}},"required":["sourceId","beforeMsgId","task"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.delete": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.rename": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"title":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.archive": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.unarchive": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.attachWorkspace": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"workspaceId":{"type":"string"}},"required":["sessionId","workspaceId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.listArchived": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["session:read"] },
  "session.execute": { params: {"type":"object","properties":{"task":{"type":"string"},"sessionId":{"type":"string"},"executionMode":{"type":"string"},"agent":{"type":"string"},"isolation":{"type":"boolean"},"agentPreset":{"type":"string"},"permissionPreset":{"type":"string"}},"required":["task"],"additionalProperties":false}, result: {"type":"object","properties":{"subscription":{"type":"string"}},"required":["subscription"],"additionalProperties":false}, streaming: true, permissions: ["session:execute"] },
  "session.cancel": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "session.steer": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"task":{"type":"string"}},"required":["sessionId","task"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "session.queueList": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"}},"required":[],"additionalProperties":false}}, streaming: false, permissions: ["session:execute"] },
  "session.queueAdd": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"text":{"type":"string"}},"required":["sessionId","text"],"additionalProperties":false}, result: {"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}, streaming: false, permissions: ["session:execute"] },
  "session.queueUpdate": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"messageId":{"type":"string"},"text":{"type":"string"}},"required":["sessionId","messageId","text"],"additionalProperties":false}, result: {"type":"object","properties":{"updated":{"type":"boolean"}},"required":["updated"],"additionalProperties":false}, streaming: false, permissions: ["session:execute"] },
  "session.queueRemove": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"messageId":{"type":"string"}},"required":["sessionId","messageId"],"additionalProperties":false}, result: {"type":"object","properties":{"removed":{"type":"boolean"}},"required":["removed"],"additionalProperties":false}, streaming: false, permissions: ["session:execute"] },
  "session.queueFlush": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {"type":"object","properties":{"flushed":{"type":"boolean"}},"required":["flushed"],"additionalProperties":false}, streaming: false, permissions: ["session:execute"] },
  "session.queueFlushNow": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {"type":"object","properties":{"flushed":{"type":"boolean"}},"required":["flushed"],"additionalProperties":false}, streaming: false, permissions: ["session:execute"] },
  "terminal.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "terminal.kill": { params: {"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "doctor.run": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "review.run": { params: {"type":"object","properties":{"target":{},"dimensions":{},"cwd":{"type":"string"}},"required":["target"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "goal.set": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"objective":{"type":"string"},"tokenBudget":{"type":"integer"}},"required":["sessionId","objective"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "goal.get": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "goal.clear": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:execute"] },
  "session.setExecutionMode": { params: {"type":"object","properties":{"mode":{"type":"string"}},"required":["mode"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.getExecutionMode": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.setApprovalMode": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"mode":{"type":"string"}},"required":["sessionId","mode"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.setPermissionPreset": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"preset":{"type":"string"}},"required":["sessionId","preset"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.getPermission": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "settings.get": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "settings.update": { params: {"type":"object","properties":{"patch":{"type":"object"}},"required":["patch"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["models:write"] },
  "session.confirmPlan": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.search": { params: {"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "sessions.running": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{"type":"string"}}, streaming: false, permissions: ["session:read"] },
  "session.timeline": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["session:read"] },
  "session.trace": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.archiveTrace": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.compact": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"keepRecent":{"type":"integer"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "session.usage": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "session.export": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"cwd":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "approval.respond": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"allowed":{"type":"boolean"},"remember":{"type":"boolean"}},"required":["sessionId","allowed"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["approval:respond"] },
  "plan.reviewDecision": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"requestId":{"type":"string"},"approved":{"type":"boolean"},"feedback":{"type":"string"}},"required":["sessionId","approved"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "interaction.respond": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"questionId":{"type":"string"},"answer":{"type":"string"}},"required":["sessionId","questionId","answer"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["approval:respond"] },
  "approval.listRules": { params: {"type":"object","properties":{"sessionId":{"type":"string"}},"required":["sessionId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:read"] },
  "approval.revokeRule": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"ruleId":{"type":"string"}},"required":["sessionId","ruleId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["session:write"] },
  "agentPresets.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"object","properties":{"presets":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"agent":{"type":"string"},"name":{"type":"string"},"description":{"type":"string"},"source":{"type":"string"},"broken":{"type":"string"},"isDefault":{"type":"boolean"}},"required":["id","agent","name","description","source","isDefault"],"additionalProperties":false}},"default":{"type":"string"}},"required":["presets","default"],"additionalProperties":false}, streaming: false, permissions: ["agents:read"] },
  "tools.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["tools:read"] },
  "agents.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["agents:read"] },
  "agents.scan": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["agents:read"] },
  "agents.create": { params: {"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"}},"required":["name","description"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["agents:write"] },
  "agents.export": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["agents:read"] },
  "agents.bindings": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["models:read"] },
  "agents.bindings.setActive": { params: {"type":"object","properties":{"agentName":{"type":"string"},"providerId":{"type":"string"},"variantId":{"type":"string"}},"required":["agentName","providerId","variantId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["models:write"] },
  "agents.bindings.setReasoningEffort": { params: {"type":"object","properties":{"agentName":{"type":"string"},"effort":{"type":"string"}},"required":["agentName","effort"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["models:write"] },
  "skills.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:read"] },
  "skills.scan": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:read"] },
  "skills.create": { params: {"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"command":{"type":"string"},"body":{"type":"string"}},"required":["name","description","body"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:write"] },
  "skills.update": { params: {"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"command":{"type":"string"},"body":{"type":"string"}},"required":["name","body"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:write"] },
  "skills.delete": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["skills:write"] },
  "skills.setAlwaysOn": { params: {"type":"object","properties":{"name":{"type":"string"},"alwaysOn":{"type":"boolean"}},"required":["name","alwaysOn"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:write"] },
  "skills.usage": { params: {"type":"object","properties":{"limit":{"type":"integer"}},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["skills:read"] },
  "hooks.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["hooks:read"] },
  "hooks.read": { params: {"type":"object","properties":{"event":{"type":"string"},"name":{"type":"string"}},"required":["event","name"],"additionalProperties":false}, result: {"type":"object","properties":{"content":{"type":"string"}},"required":["content"],"additionalProperties":false}, streaming: false, permissions: ["hooks:read"] },
  "hooks.write": { params: {"type":"object","properties":{"event":{"type":"string"},"name":{"type":"string"},"content":{"type":"string"}},"required":["event","name","content"],"additionalProperties":false}, result: {"type":"object","properties":{},"required":[],"additionalProperties":false}, streaming: false, permissions: ["hooks:write"] },
  "hooks.delete": { params: {"type":"object","properties":{"event":{"type":"string"},"name":{"type":"string"}},"required":["event","name"],"additionalProperties":false}, result: {"type":"object","properties":{},"required":[],"additionalProperties":false}, streaming: false, permissions: ["hooks:write"] },
  "plugins.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["plugins:read"] },
  "plugins.install": { params: {"type":"object","properties":{"source":{"type":"string"}},"required":["source"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["plugins:write"] },
  "plugins.uninstall": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["plugins:write"] },
  "models.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["models:read"] },
  "models.providers.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["models:read"] },
  "models.add": { params: {"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"modelType":{"type":"string"},"providerType":{"type":"string"},"baseUrl":{"type":"string"},"apiKey":{"type":"string"},"modelPath":{"type":"string"},"defaultModel":{"type":"string"},"variants":{"type":"array","items":{}},"concurrency":{"type":"integer"},"maxTokens":{"type":"integer"},"contextWindow":{"type":"integer"},"temperature":{"type":"number"},"apiFormat":{"type":"string"},"reasoningEffort":{"type":"string"},"modelLabel":{"type":"string"},"supportsVision":{"type":"boolean"}},"required":["id","name","modelType","providerType","defaultModel"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["models:write"] },
  "models.remove": { params: {"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["models:write"] },
  "configurableProviders.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["models:read"] },
  "subagents.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["agents:read"] },
  "subagents.set": { params: {"type":"object","properties":{"name":{"type":"string"},"enabled":{"type":"boolean"},"model":{"type":"string"}},"required":["name","enabled"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["agents:write"] },
  "credentials.describe": { params: {"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["credentials:read"] },
  "credentials.read": { params: {"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}, result: {"type":"string"}, streaming: false, permissions: ["credentials:read"] },
  "credentials.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["credentials:read"] },
  "credentials.setApiKey": { params: {"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["credentials:write"] },
  "credentials.unset": { params: {"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["credentials:write"] },
  "memory.list": { params: {"type":"object","properties":{"projectId":{"type":"string"}},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["memory:read"] },
  "memory.recall": { params: {"type":"object","properties":{"projectId":{"type":"string"},"query":{"type":"string"}},"required":["query"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["memory:read"] },
  "memory.upsert": { params: {"type":"object","properties":{"projectId":{"type":"string"},"name":{"type":"string"},"description":{"type":"string"},"body":{"type":"string"}},"required":["name","description","body"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:write"] },
  "memory.delete": { params: {"type":"object","properties":{"projectId":{"type":"string"},"name":{"type":"string"}},"required":["projectId","name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:write"] },
  "memory.clear": { params: {"type":"object","properties":{"projectId":{"type":"string"}},"required":["projectId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:write"] },
  "memory.ingest": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:write"] },
  "memory.status": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:read"] },
  "memory.export": { params: {"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["memory:write"] },
  "mcp.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["mcp:read"] },
  "mcp.add": { params: {"type":"object","properties":{"name":{"type":"string"},"transport":{"type":"string"},"command":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"env":{},"url":{"type":"string"},"oauth":{}},"required":["name","transport"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:write"] },
  "mcp.remove": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:write"] },
  "mcp.refresh": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:write"] },
  "mcp.oauth.connect": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:write"] },
  "mcp.oauth.disconnect": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:write"] },
  "mcp.testServer": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:read"] },
  "mcp.testTool": { params: {"type":"object","properties":{"name":{"type":"string"},"toolName":{"type":"string"}},"required":["name","toolName"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["mcp:read"] },
  "scheduled.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["scheduled:read"] },
  "scheduled.add": { params: {"type":"object","properties":{"name":{"type":"string"},"mode":{},"prompt":{"type":"string"},"model":{"type":"string"},"agent":{"type":"string"},"workspace":{"type":"string"},"enabled":{"type":"boolean"},"startDate":{"type":"string"},"endDate":{"type":"string"},"skills":{"type":"array","items":{"type":"string"}}},"required":["name","mode","prompt"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["scheduled:write"] },
  "scheduled.update": { params: {"type":"object","properties":{"name":{"type":"string"},"mode":{},"prompt":{"type":"string"},"model":{"type":"string"},"agent":{"type":"string"},"workspace":{"type":"string"},"enabled":{"type":"boolean"},"startDate":{"type":"string"},"endDate":{"type":"string"},"skills":{"type":"array","items":{"type":"string"}}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["scheduled:write"] },
  "scheduled.remove": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["scheduled:write"] },
  "scheduled.runNow": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["scheduled:write"] },
  "jobs.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["jobs:read"] },
  "jobs.taskRuns": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["jobs:read"] },
  "jobs.taskRun.history": { params: {"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["jobs:read"] },
  "market.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["market:read"] },
  "workspace.instructions": { params: {"type":"object","properties":{"workspaceId":{"type":"string"}},"required":["workspaceId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "workspace.diff": { params: {"type":"object","properties":{"cwd":{"type":"string"}},"required":["cwd"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "workspace.diffStat": { params: {"type":"object","properties":{"cwd":{"type":"string"}},"required":["cwd"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "workspace.worktreeStatus": { params: {"type":"object","properties":{"cwd":{"type":"string"}},"required":["cwd"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "project.agents.init": { params: {"type":"object","properties":{"cwd":{"type":"string"}},"required":["cwd"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["project:write"] },
  "review.start": { params: {"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}},"dimensions":{"type":"array","items":{"type":"string"}}},"required":["files"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["review:start"] },
  "attachments.admit": { params: {"type":"object","properties":{"images":{"type":"array","items":{}}},"required":["images"],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["attachments:write"] },
  "attachments.read": { params: {"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["attachments:read"] },
  "feedback.record": { params: {"type":"object","properties":{"sessionId":{"type":"string"},"kind":{"type":"string"},"comment":{"type":"string"}},"required":["sessionId","kind"],"additionalProperties":false}, result: {"type":"object","properties":{"ok":{"type":"boolean"}},"required":[],"additionalProperties":false}, streaming: false, permissions: ["session:write"] },
  "fs.suggest": { params: {"type":"object","properties":{"workspaceId":{"type":"string"},"prefix":{"type":"string"}},"required":["workspaceId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "workspaces.list": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {"type":"array","items":{}}, streaming: false, permissions: ["workspaces:read"] },
  "workspaces.create": { params: {"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:write"] },
  "workspaces.rename": { params: {"type":"object","properties":{"workspaceId":{"type":"string"},"title":{"type":"string"}},"required":["workspaceId","title"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:write"] },
  "workspaces.active": { params: {"type":"object","properties":{"agent":{"type":"string"}},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:read"] },
  "workspaces.delete": { params: {"type":"object","properties":{"workspaceId":{"type":"string"}},"required":["workspaceId"],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:write"] },
  "workspaces.activate": { params: {"type":"object","properties":{"workspaceId":{"type":"string"},"agent":{"type":"string"}},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: ["workspaces:write"] },
  "protocol.info": { params: {"type":"object","properties":{},"required":[],"additionalProperties":false}, result: {}, streaming: false, permissions: [] },
};

export interface ProtocolClientMethods {
  sessionList(params: { workspaceId?: string }): Promise<unknown[]>;
  sessionMessages(params: { sessionId: string; limit?: number; before?: number }): Promise<unknown>;
  sessionArchiveMessages(params: { sessionId: string }): Promise<unknown>;
  sessionFork(params: { sourceId: string; beforeMsgId: number; task: string }): Promise<unknown>;
  sessionDelete(params: { sessionId: string }): Promise<unknown>;
  sessionRename(params: { sessionId: string; title?: string }): Promise<unknown>;
  sessionArchive(params: { sessionId: string }): Promise<unknown>;
  sessionUnarchive(params: { sessionId: string }): Promise<unknown>;
  sessionAttachWorkspace(params: { sessionId: string; workspaceId: string }): Promise<unknown>;
  sessionListArchived(params: {  }): Promise<unknown[]>;
  sessionExecute(params: { task: string; sessionId?: string; executionMode?: string; agent?: string; isolation?: boolean; agentPreset?: string; permissionPreset?: string }): Promise<{ subscription: string }>;
  sessionCancel(params: { sessionId: string }): Promise<unknown>;
  sessionSteer(params: { sessionId: string; task: string }): Promise<unknown>;
  sessionQueueList(params: { sessionId: string }): Promise<{ id?: string; text?: string }[]>;
  sessionQueueAdd(params: { sessionId: string; text: string }): Promise<{ id: string }>;
  sessionQueueUpdate(params: { sessionId: string; messageId: string; text: string }): Promise<{ updated: boolean }>;
  sessionQueueRemove(params: { sessionId: string; messageId: string }): Promise<{ removed: boolean }>;
  sessionQueueFlush(params: { sessionId: string }): Promise<{ flushed: boolean }>;
  sessionQueueFlushNow(params: { sessionId: string }): Promise<{ flushed: boolean }>;
  terminalList(params: {  }): Promise<unknown>;
  terminalKill(params: { id: string }): Promise<unknown>;
  doctorRun(params: {  }): Promise<unknown>;
  reviewRun(params: { target: unknown; dimensions?: unknown; cwd?: string }): Promise<unknown>;
  goalSet(params: { sessionId: string; objective: string; tokenBudget?: number }): Promise<unknown>;
  goalGet(params: { sessionId: string }): Promise<unknown>;
  goalClear(params: { sessionId: string }): Promise<unknown>;
  sessionSetExecutionMode(params: { mode: string }): Promise<unknown>;
  sessionGetExecutionMode(params: { sessionId?: string }): Promise<unknown>;
  sessionSetApprovalMode(params: { sessionId: string; mode: string }): Promise<unknown>;
  sessionSetPermissionPreset(params: { sessionId: string; preset: string }): Promise<unknown>;
  sessionGetPermission(params: { sessionId: string }): Promise<unknown>;
  settingsGet(params: {  }): Promise<unknown>;
  settingsUpdate(params: { patch: {  } }): Promise<unknown>;
  sessionConfirmPlan(params: { sessionId: string }): Promise<unknown>;
  sessionSearch(params: { query: string; limit?: number }): Promise<unknown>;
  sessionsRunning(params: {  }): Promise<string[]>;
  sessionTimeline(params: { sessionId: string }): Promise<unknown[]>;
  sessionTrace(params: { sessionId: string }): Promise<unknown>;
  sessionArchiveTrace(params: { sessionId: string }): Promise<unknown>;
  sessionCompact(params: { sessionId: string; keepRecent?: number }): Promise<unknown>;
  sessionUsage(params: { sessionId: string }): Promise<unknown>;
  sessionExport(params: { sessionId: string; cwd?: string }): Promise<unknown>;
  approvalRespond(params: { sessionId: string; allowed: boolean; remember?: boolean }): Promise<unknown>;
  planReviewDecision(params: { sessionId: string; requestId?: string; approved: boolean; feedback?: string }): Promise<unknown>;
  interactionRespond(params: { sessionId: string; questionId: string; answer: string }): Promise<unknown>;
  approvalListRules(params: { sessionId: string }): Promise<unknown>;
  approvalRevokeRule(params: { sessionId: string; ruleId: string }): Promise<unknown>;
  agentPresetsList(params: {  }): Promise<{ presets: { id: string; agent: string; name: string; description: string; source: string; broken?: string; isDefault: boolean }[]; default: string }>;
  toolsList(params: {  }): Promise<unknown[]>;
  agentsList(params: {  }): Promise<unknown[]>;
  agentsScan(params: {  }): Promise<unknown[]>;
  agentsCreate(params: { name: string; description: string }): Promise<unknown>;
  agentsExport(params: { name: string }): Promise<unknown>;
  agentsBindings(params: {  }): Promise<unknown[]>;
  agentsBindingsSetActive(params: { agentName: string; providerId: string; variantId: string }): Promise<unknown>;
  agentsBindingsSetReasoningEffort(params: { agentName: string; effort: string }): Promise<unknown>;
  skillsList(params: {  }): Promise<unknown[]>;
  skillsScan(params: {  }): Promise<unknown[]>;
  skillsCreate(params: { name: string; description: string; command?: string; body: string }): Promise<unknown[]>;
  skillsUpdate(params: { name: string; description?: string; command?: string; body: string }): Promise<unknown[]>;
  skillsDelete(params: { name: string }): Promise<unknown>;
  skillsSetAlwaysOn(params: { name: string; alwaysOn: boolean }): Promise<unknown[]>;
  skillsUsage(params: { limit?: number }): Promise<unknown[]>;
  hooksList(params: {  }): Promise<unknown[]>;
  hooksRead(params: { event: string; name: string }): Promise<{ content: string }>;
  hooksWrite(params: { event: string; name: string; content: string }): Promise<{  }>;
  hooksDelete(params: { event: string; name: string }): Promise<{  }>;
  pluginsList(params: {  }): Promise<unknown[]>;
  pluginsInstall(params: { source: string }): Promise<unknown>;
  pluginsUninstall(params: { name: string }): Promise<unknown[]>;
  modelsList(params: {  }): Promise<unknown[]>;
  modelsProvidersList(params: {  }): Promise<unknown[]>;
  modelsAdd(params: { id: string; name: string; modelType: string; providerType: string; baseUrl?: string; apiKey?: string; modelPath?: string; defaultModel: string; variants?: unknown[]; concurrency?: number; maxTokens?: number; contextWindow?: number; temperature?: number; apiFormat?: string; reasoningEffort?: string; modelLabel?: string; supportsVision?: boolean }): Promise<unknown>;
  modelsRemove(params: { id: string }): Promise<unknown>;
  configurableProvidersList(params: {  }): Promise<unknown[]>;
  subagentsList(params: {  }): Promise<unknown[]>;
  subagentsSet(params: { name: string; enabled: boolean; model?: string }): Promise<unknown>;
  credentialsDescribe(params: { key: string }): Promise<unknown>;
  credentialsRead(params: { key: string }): Promise<string>;
  credentialsList(params: {  }): Promise<unknown[]>;
  credentialsSetApiKey(params: { key: string; value: string }): Promise<unknown>;
  credentialsUnset(params: { key: string }): Promise<unknown>;
  memoryList(params: { projectId?: string }): Promise<unknown[]>;
  memoryRecall(params: { projectId?: string; query: string }): Promise<unknown[]>;
  memoryUpsert(params: { projectId?: string; name: string; description: string; body: string }): Promise<unknown>;
  memoryDelete(params: { projectId: string; name: string }): Promise<unknown>;
  memoryClear(params: { projectId: string }): Promise<unknown>;
  memoryIngest(params: {  }): Promise<unknown>;
  memoryStatus(params: {  }): Promise<unknown>;
  memoryExport(params: { path: string }): Promise<unknown>;
  mcpList(params: {  }): Promise<unknown[]>;
  mcpAdd(params: { name: string; transport: string; command?: string; args?: string[]; env?: unknown; url?: string; oauth?: unknown }): Promise<unknown>;
  mcpRemove(params: { name: string }): Promise<unknown>;
  mcpRefresh(params: { name?: string }): Promise<unknown>;
  mcpOauthConnect(params: { name: string }): Promise<unknown>;
  mcpOauthDisconnect(params: { name: string }): Promise<unknown>;
  mcpTestServer(params: { name: string }): Promise<unknown>;
  mcpTestTool(params: { name: string; toolName: string }): Promise<unknown>;
  scheduledList(params: {  }): Promise<unknown[]>;
  scheduledAdd(params: { name: string; mode: unknown; prompt: string; model?: string; agent?: string; workspace?: string; enabled?: boolean; startDate?: string; endDate?: string; skills?: string[] }): Promise<unknown>;
  scheduledUpdate(params: { name: string; mode?: unknown; prompt?: string; model?: string; agent?: string; workspace?: string; enabled?: boolean; startDate?: string; endDate?: string; skills?: string[] }): Promise<unknown>;
  scheduledRemove(params: { name: string }): Promise<unknown>;
  scheduledRunNow(params: { name: string }): Promise<unknown>;
  jobsList(params: {  }): Promise<unknown[]>;
  jobsTaskRuns(params: {  }): Promise<unknown[]>;
  jobsTaskRunHistory(params: { name: string }): Promise<unknown[]>;
  marketList(params: {  }): Promise<unknown[]>;
  workspaceInstructions(params: { workspaceId: string }): Promise<unknown>;
  workspaceDiff(params: { cwd: string }): Promise<unknown>;
  workspaceDiffStat(params: { cwd: string }): Promise<unknown>;
  workspaceWorktreeStatus(params: { cwd: string }): Promise<unknown>;
  projectAgentsInit(params: { cwd: string }): Promise<unknown>;
  reviewStart(params: { files: string[]; dimensions?: string[] }): Promise<unknown>;
  attachmentsAdmit(params: { images: unknown[] }): Promise<unknown[]>;
  attachmentsRead(params: { id: string }): Promise<unknown>;
  feedbackRecord(params: { sessionId: string; kind: string; comment?: string }): Promise<{ ok?: boolean }>;
  fsSuggest(params: { workspaceId: string; prefix?: string }): Promise<unknown>;
  workspacesList(params: {  }): Promise<unknown[]>;
  workspacesCreate(params: { path: string }): Promise<unknown>;
  workspacesRename(params: { workspaceId: string; title: string }): Promise<unknown>;
  workspacesActive(params: { agent?: string }): Promise<unknown>;
  workspacesDelete(params: { workspaceId: string }): Promise<unknown>;
  workspacesActivate(params: { workspaceId?: string; agent?: string }): Promise<unknown>;
  protocolInfo(params: {  }): Promise<unknown>;
}
