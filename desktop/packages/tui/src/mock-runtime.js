export class MockAgentRuntime {
  constructor() { console.log("[Mock] AgentRuntime initialized"); }
  execute(task, cb) { console.log("[Mock] execute:", task); cb(null, {type:"text","data":"mock"}); }
  listAgents() { return []; }
  scanAgents() { return []; }
  switchAgent(n) { console.log("[Mock] switchAgent:", n); }
  getCurrentAgent() { return null; }
  listTools() { return [{name:"read",description:"mock",requiresApproval:false}]; }
  listSkills() { return []; }
  scanSkills() { return []; }
  shutdown() {}
}
