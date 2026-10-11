// starriver-memory-server — stdio MCP server for the shared memory store.
//
// Launch (agents configured with `command: starriver-memory-server`):
//   starriver-memory-server [--root <dir>] [--project <id>] [--agent <name>]
//
// Defaults: root `~/.starriver/memory`, project derived from the process cwd,
// source agent tag "mcp-client" (memories written through the MCP bridge).

use std::path::PathBuf;

use starriver_memory::{MemoryConfig, MemoryStore};

fn main() -> anyhow::Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut project: Option<String> = None;
    let mut agent = "mcp-client".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = args.next().map(PathBuf::from),
            "--project" => project = args.next(),
            "--agent" => agent = args.next().unwrap_or_else(|| agent.clone()),
            _ => {}
        }
    }
    let root = root.unwrap_or_else(starriver_memory_server::default_root);
    let project = project.unwrap_or_else(|| {
        starriver_memory_server::project_from_cwd(std::env::current_dir().ok().as_deref())
    });
    let store = MemoryStore::new(root, MemoryConfig::default());
    starriver_memory_server::run(store, project, agent)
}
