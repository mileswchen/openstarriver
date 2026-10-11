//! `starriver-sbx-helper` — Windows confined-run wrapper (WI-73).
//!
//! Builds on every platform as an empty binary so the workspace stays
//! cross-platform and cargo keeps the helper next to the other target-dir
//! bins; the real CLI lives in `starriver_sandbox::windows::cli_main` and
//! only compiles for Windows. Usage:
//!
//! ```text
//! starriver-sbx-helper probe
//! starriver-sbx-helper run <read-only|workspace-write> <root> -- <argv…>
//! ```

fn main() {
    #[cfg(target_os = "windows")]
    {
        let code = starriver_sandbox::windows::cli_main();
        std::process::exit(code);
    }
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("starriver-sbx-helper is only functional on Windows");
        std::process::exit(1);
    }
}
