export interface ToolContext {
  callTool(name: string, params: Record<string, any>): Promise<{ output: string }>;
  workspace: string; session: string; env: Record<string, string>;
}
export interface ToolConfig<P = Record<string, any>> {
  name: string; description: string;
  params?: Record<string, { type: string; required?: boolean; default?: any }>;
  execute(params: P, ctx: ToolContext): Promise<any>;
}
export interface SkillConfig {
  name: string; description: string; body: string;
  tools?: string[]; alwaysOn?: boolean; command?: string;
}
export interface AgentConfig {
  name: string; description: string; body: string;
  skills?: string[]; tools?: string[];
  model?: { default: string; fallback?: string };
}
export function defineTool<P = Record<string, any>>(config: ToolConfig<P>): ToolConfig<P> { return config; }
export function defineSkill(config: SkillConfig): SkillConfig { return config; }
export function defineAgent(config: AgentConfig): AgentConfig { return config; }
