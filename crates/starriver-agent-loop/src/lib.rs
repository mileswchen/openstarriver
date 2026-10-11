//! Default agent-loop driver: registry implementation, concrete loop agent,
//! and the tool-call scheduler, matching `the agent loop`.

pub mod agent;
pub mod agent_loop;
pub mod approval;
pub mod rules_file;
pub mod context;
pub mod event_mapper;
pub mod image_projection;
pub mod mcp_status;
pub mod instructions;
pub mod registry;
pub mod services;
pub mod time_context;
pub mod tool_calls;
