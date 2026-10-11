//! Sandbox re-export shim: implementations live in `starriver-sandbox`
//! (Seatbelt on macOS; bubblewrap + Landlock probe chain and denial
//! normalization on Linux; write-restricted token via the
//! `starriver-sbx-helper` wrapper on Windows). Host call sites keep their
//! historical paths.

pub use starriver_sandbox::{effective_workspace_root, ensure_workspace_writable, SystemSandbox};
