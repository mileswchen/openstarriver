//! Language Server Protocol integration for the Coding Agent.
//!
//! A tiny LSP *client* speaking Content-Length-framed JSON-RPC over stdio to
//! language servers. [`LspManager`] keeps one server process per (language,
//! workspace root) so repeated queries on the same project reuse a warm
//! server; [`ServerSpec`] describes which server handles which file
//! extensions and how to find its project root.
//!
//! Built-in servers: rust-analyzer (Rust), typescript-language-server
//! (TS/JS), pyright (Python), gopls (Go), clangd (C/C++). Deployments
//! override or extend the registry through `settings.yaml` (`lsp.servers`).
//! Servers that are not installed degrade to a clear error carrying the
//! install hint; everything else keeps working.
//!
//! Validated by the standalone connectivity spike at /tmp/lsp-spike; findings
//! baked in here: rust-analyzer only type-checks files inside a real cargo
//! project (root discovery via [`find_root`]), diagnostics arrive in
//! multiple pushes (a 0-item clear then the real set), and nested workspaces
//! under a different root are not loaded reliably.

pub mod client;
pub mod transport;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use lsp_types::Uri;

use client::LspClient;

/// Build a `file://` URI from an absolute path (percent-encoding each segment).
/// lsp-types 0.97's `Uri` only implements `FromStr` — no `from_file_path`.
pub fn file_uri(p: &Path) -> Uri {
    /// One path component as a URI segment (`/` + percent-encoded bytes).
    /// Unreserved URI characters (plus `:`) stay literal; a Windows drive
    /// prefix yields the vscode-style `file:///C:/Users/…` form servers emit.
    fn segment(s: &mut String, bytes: &[u8]) {
        // RootDir already ends `file://` in `/`; don't double it.
        if !s.ends_with('/') {
            s.push('/');
        }
        for b in bytes {
            if b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'_'
                        | b'.'
                        | b'~'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                        | b':'
                )
            {
                s.push(*b as char);
            } else {
                s.push_str(&format!("%{b:02X}"));
            }
        }
    }

    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    // Windows canonicalize yields the `\\?\C:\…` verbatim form; strip it so
    // the drive prefix encodes as a clean `C:` segment (`file:///C:/…`).
    let plain = canonical.to_string_lossy();
    let plain = plain.strip_prefix(r"\\?\").unwrap_or(plain.as_ref());
    let mut s = String::from("file://");
    for comp in std::path::Path::new(plain).components() {
        match comp {
            Component::Prefix(pfx) => segment(&mut s, pfx.as_os_str().to_string_lossy().as_bytes()),
            Component::RootDir => s.push('/'),
            Component::Normal(c) => segment(&mut s, c.to_string_lossy().as_bytes()),
            _ => {}
        }
    }
    s.parse::<Uri>().expect("valid file:// uri")
}

/// One language server in the registry: which files it handles, how to spawn
/// it, and where its project root lives.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// Registry key ("rust", "typescript", …); custom entries with the same
    /// id replace a builtin.
    pub id: String,
    /// File extensions (without dot) routed to this server.
    pub extensions: Vec<String>,
    /// Server binary on PATH (overridable via `STARRIVER_LSP_BIN_<ID>`).
    pub command: String,
    /// Command-line arguments (e.g. `--stdio`).
    pub args: Vec<String>,
    /// File names that mark a project root; the TOPMOST ancestor containing
    /// the first marker wins (monorepo root over nested package).
    pub root_markers: Vec<String>,
    /// LSP language id sent in `textDocument/didOpen`.
    pub language_id: String,
    /// Shown when spawning fails — the install command.
    pub install_hint: String,
}

impl ServerSpec {
    pub fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| self.extensions.iter().any(|listed| listed == ext))
    }
}

/// The shipped server registry.
pub fn builtin_servers() -> Vec<ServerSpec> {
    let server = |id: &str,
                  extensions: &[&str],
                  command: &str,
                  args: &[&str],
                  root_markers: &[&str],
                  language_id: &str,
                  install_hint: &str| ServerSpec {
        id: id.into(),
        extensions: extensions.iter().map(|e| e.to_string()).collect(),
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        root_markers: root_markers.iter().map(|m| m.to_string()).collect(),
        language_id: language_id.into(),
        install_hint: install_hint.into(),
    };
    vec![
        server(
            "rust",
            &["rs"],
            "rust-analyzer",
            &[],
            &["Cargo.toml"],
            "rust",
            "rustup component add rust-analyzer",
        ),
        server(
            "typescript",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            "typescript-language-server",
            &["--stdio"],
            &["package.json", "tsconfig.json"],
            "typescript",
            "npm install -g typescript typescript-language-server",
        ),
        server(
            "python",
            &["py", "pyi"],
            "pyright-langserver",
            &["--stdio"],
            &["pyproject.toml", "setup.py", "requirements.txt"],
            "python",
            "npm install -g pyright (or pip install pyright)",
        ),
        server(
            "go",
            &["go"],
            "gopls",
            &[],
            &["go.mod"],
            "go",
            "go install golang.org/x/tools/gopls@latest",
        ),
        server(
            "cpp",
            &["c", "h", "cpp", "hpp", "cc", "hh", "cxx"],
            "clangd",
            &[],
            &["compile_commands.json", "CMakeLists.txt", "Makefile"],
            "c++",
            "brew install llvm (clangd) or apt install clangd",
        ),
    ]
}

/// Walk up from `path`; the TOPMOST ancestor containing the first root marker
/// is the project root (a monorepo workspace root outranks its nested
/// packages), falling back to the topmost ancestor with any other marker.
pub fn find_root(path: &Path, root_markers: &[String]) -> Option<PathBuf> {
    let Some(primary) = root_markers.first() else {
        return None;
    };
    let mut dir = path.parent()?;
    let mut primary_hit: Option<PathBuf> = None;
    let mut any_hit: Option<PathBuf> = None;
    loop {
        for marker in root_markers {
            if dir.join(marker).is_file() {
                if marker == primary {
                    // The walk ascends, so every hit is shallower than the
                    // last — keep overwriting to land on the TOPMOST marker.
                    primary_hit = Some(dir.to_path_buf());
                } else {
                    any_hit = Some(dir.to_path_buf());
                }
                break;
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return primary_hit.or(any_hit),
        }
    }
}

/// Walk up from `path` to find the rust project root: the topmost directory
/// holding a `Cargo.toml` (a `[workspace]` root outranks nested packages).
///
/// rust-analyzer only produces type diagnostics / resolves definitions for
/// files that belong to a real crate graph, so this directory is what the
/// server gets as its project root.
pub fn find_workspace_root(path: &Path) -> Option<PathBuf> {
    find_root(path, &["Cargo.toml".to_string()])
}

/// Whether any registered server handles this file.
pub fn is_supported_file(path: &Path) -> bool {
    builtin_servers().iter().any(|spec| spec.supports(path))
}

/// How long a session may sit unused before the next access reaps it.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

struct Entry {
    client: Arc<LspClient>,
    last_used: Instant,
}

/// One server process per (language, workspace root), created lazily on first
/// use.
///
/// Sessions idle for [`DEFAULT_IDLE_TIMEOUT`] are shut down (graceful
/// `shutdown` → `exit`) on the next `session_for_file`, so a long-lived agent
/// that touches many projects doesn't accumulate one server process each.
pub struct LspManager {
    specs: Vec<ServerSpec>,
    sessions: tokio::sync::Mutex<HashMap<(String, PathBuf), Entry>>,
    idle_timeout: Duration,
}

impl Default for LspManager {
    fn default() -> Self {
        Self::with_specs(builtin_servers())
    }
}

impl LspManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry built from `settings.yaml` (`lsp.servers`): entries replace
    /// builtins with the same id, others extend the registry.
    pub fn with_specs(specs: Vec<ServerSpec>) -> Self {
        Self {
            specs,
            sessions: Default::default(),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// Override the idle timeout (tests use a tiny value).
    #[cfg(test)]
    pub fn with_idle_timeout(idle_timeout: Duration) -> Self {
        Self {
            specs: builtin_servers(),
            sessions: Default::default(),
            idle_timeout,
        }
    }

    /// The server spec that handles `path`, if any.
    pub fn spec_for_file(&self, path: &Path) -> Option<&ServerSpec> {
        self.specs.iter().find(|spec| spec.supports(path))
    }

    /// Register a pre-built client under `(spec_id, root)`. Embedders (and
    /// the tool tests) use this to supply a session over a custom transport
    /// instead of a spawned process; `session_for_file` then reuses it like
    /// any warm session.
    pub async fn register_session(&self, spec_id: &str, root: &Path, client: Arc<LspClient>) {
        self.sessions.lock().await.insert(
            (spec_id.to_string(), root.to_path_buf()),
            Entry {
                client,
                last_used: Instant::now(),
            },
        );
    }

    /// Get — or spawn + initialize — the server session for the file. The
    /// lock is held across the spawn so a single session is never
    /// double-instantiated; idle-eviction shutdowns run after the lock is
    /// released so a hung server cannot stall unrelated calls.
    pub async fn session_for_file(&self, path: &Path) -> Result<(Arc<LspClient>, ServerSpec)> {
        let Some(spec) = self.spec_for_file(path) else {
            anyhow::bail!("no language server registered for {}", path.display());
        };
        let Some(root) = find_root(path, &spec.root_markers) else {
            anyhow::bail!(
                "no project root for {} (looked for {} on parent directories)",
                path.display(),
                spec.root_markers.join("/")
            );
        };
        let key = (spec.id.clone(), root.clone());
        let mut sessions = self.sessions.lock().await;
        let victims = self.take_idle(&mut sessions);
        if let Some(e) = sessions.get_mut(&key) {
            if !e.client.is_reader_dead() {
                e.last_used = Instant::now();
                let client = e.client.clone();
                drop(sessions);
                for victim in victims {
                    victim.client.shutdown().await;
                }
                return Ok((client, spec.clone()));
            }
            // Crashed server: drop the corpse and respawn — and do NOT let
            // the failing calls refresh last_used into a livelock.
            sessions.remove(&key);
        }
        let (stdin, stdout, child) = match LspClient::spawn_process(root.as_path(), spec).await {
            Ok(parts) => parts,
            Err(error) => {
                anyhow::bail!(
                    "language server {} ({}{}) could not start: {error:#}; install hint: {}",
                    spec.id,
                    spec.command,
                    spec.args.join(" "),
                    spec.install_hint
                );
            }
        };
        let client = LspClient::spawn(&root, &spec.language_id, stdin, stdout, Some(child)).await?;
        sessions.insert(
            key,
            Entry {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        drop(sessions);
        for victim in victims {
            victim.client.shutdown().await;
        }
        Ok((client, spec.clone()))
    }

    /// Remove every session idle past `idle_timeout` from the map and return
    /// them for out-of-lock graceful shutdown.
    fn take_idle(
        &self,
        sessions: &mut HashMap<(String, PathBuf), Entry>,
    ) -> Vec<Entry> {
        let now = Instant::now();
        let idle: Vec<(String, PathBuf)> = sessions
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_used) > self.idle_timeout)
            .map(|(key, _)| key.clone())
            .collect();
        idle.into_iter().filter_map(|key| sessions.remove(&key)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_encodes_spaces_and_utf8() {
        let uri = file_uri(Path::new("/tmp/my dir/caf\u{e9} file.rs"));
        assert_eq!(uri.as_str(), "file:///tmp/my%20dir/caf%C3%A9%20file.rs");
    }

    #[test]
    fn file_uri_plain_path() {
        let uri = file_uri(Path::new("/tmp/lsp-ws/sample/src/main.rs"));
        assert_eq!(uri.as_str(), "file:///tmp/lsp-ws/sample/src/main.rs");
    }

    #[test]
    fn find_root_prefers_topmost_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("crate-a/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate-a\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\n",
        )
        .unwrap();
        std::fs::write(root.join("crate-a/src/main.rs"), "fn main() {}\n").unwrap();
        let file = root.join("crate-a/src/main.rs");
        assert_eq!(find_root(&file, &["Cargo.toml".to_string()]).unwrap(), root);
    }

    #[test]
    fn find_workspace_root_uses_deepest_package_when_no_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("crate-x");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(pkg.join("Cargo.toml"), "[package]\nname = \"crate-x\"\n").unwrap();
        std::fs::write(pkg.join("src/lib.rs"), "").unwrap();
        let file = pkg.join("src/lib.rs");
        assert_eq!(find_workspace_root(&file).unwrap(), pkg);
    }

    #[test]
    fn find_root_none_for_non_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("orphan.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        assert!(find_root(&file, &["Cargo.toml".to_string()]).is_none());
    }

    #[test]
    fn is_supported_file_covers_registered_languages() {
        assert!(is_supported_file(Path::new("src/main.rs")));
        assert!(is_supported_file(Path::new("src/app.tsx")));
        assert!(is_supported_file(Path::new("src/app.py")));
        assert!(is_supported_file(Path::new("main.go")));
        assert!(is_supported_file(Path::new("src/main.cpp")));
        assert!(!is_supported_file(Path::new("README.md")));
    }

    #[test]
    fn manager_default_has_no_sessions() {
        let m = LspManager::new();
        // No public accessor; just ensure construction + Default are sane.
        assert!(m.sessions.try_lock().is_ok());
    }

    #[test]
    fn custom_specs_replace_builtins_by_id() {
        let mut specs = builtin_servers();
        let ts = specs
            .iter_mut()
            .find(|s| s.id == "typescript")
            .expect("builtin ts server");
        ts.command = "/custom/tls".into();
        let manager = LspManager::with_specs(specs);
        let spec = manager
            .spec_for_file(Path::new("src/app.ts"))
            .expect("ts still supported");
        assert_eq!(spec.command, "/custom/tls");
    }

    #[tokio::test]
    async fn idle_sessions_are_evicted_and_shutdown() {
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);
        // Fake server: answer initialize + shutdown with null, stop on exit.
        let server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            loop {
                let bytes = match transport::read_frame(&mut reader).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if v.get("method").and_then(|m| m.as_str()) == Some("exit") {
                    break;
                }
                if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                    transport::write_frame(
                        &mut writer,
                        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                    )
                    .await
                    .unwrap();
                }
            }
        });

        let client = LspClient::spawn(
            tmp.path(),
            "rust",
            Box::new(to_srv),
            Box::new(from_srv),
            None,
        )
        .await
        .expect("handshake completes");
        let manager = LspManager::with_idle_timeout(Duration::from_millis(1));
        let mut sessions = HashMap::new();
        sessions.insert(
            ("rust".into(), tmp.path().to_path_buf()),
            Entry {
                client,
                last_used: Instant::now() - Duration::from_secs(60),
            },
        );
        // take_idle removes the entries; the caller shuts them down out of
        // band (the new eviction contract keeps the manager lock free).
        let victims = manager.take_idle(&mut sessions);
        for victim in victims {
            victim.client.shutdown().await;
        }
        assert!(
            sessions.is_empty(),
            "idle session should be evicted and dropped"
        );
        server.await.unwrap();
    }
}
