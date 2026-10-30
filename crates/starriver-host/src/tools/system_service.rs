//! BUG-101: commands that drive macOS system XPC services (disk images,
//! disk management, software install/update, Time Machine) FREEZE inside the
//! seatbelt sandbox — the XPC request is silently blocked, never fails, and
//! the agent turn hangs until the bash timeout. In confined mode they are
//! refused up front with guidance to run them outside the sandboxed shell.

/// Commands whose first word targets a system service that is known to
/// freeze under the seatbelt sandbox. Extensible as new freeze cases appear.
pub const SYSTEM_SERVICE_COMMANDS: &[&str] = &[
    "hdiutil",
    "diskutil",
    "diskimagesiod",
    "installer",
    "softwareupdate",
    "tmutil",
];

/// True when any command segment's first word targets a system service.
/// Segmenting mirrors `bash_readonly` (quote-aware split on control
/// operators), so `cd … && hdiutil create …` is caught too. Bare names and
/// absolute paths (`/usr/bin/hdiutil`) both match.
pub fn command_needs_system_services(command: &str) -> bool {
    for segment in crate::tools::bash_readonly::split_segments(command) {
        let first = segment.split_whitespace().next().unwrap_or("");
        let stem = first.rsplit('/').next().unwrap_or(first);
        if SYSTEM_SERVICE_COMMANDS.contains(&stem) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bare_and_pathed_and_compound_segments() {
        assert!(command_needs_system_services(
            "hdiutil create -format UDRW x.dmg"
        ));
        assert!(command_needs_system_services(
            "/usr/bin/hdiutil attach x.dmg"
        ));
        assert!(command_needs_system_services(
            "cd /repo && export PATH=/x && hdiutil create x"
        ));
        assert!(command_needs_system_services("echo hi | diskutil list"));
        assert!(command_needs_system_services("installer -pkg x.pkg"));
        assert!(command_needs_system_services("softwareupdate -l"));
        assert!(command_needs_system_services("tmutil status"));
    }

    #[test]
    fn ordinary_commands_are_not_flagged() {
        assert!(!command_needs_system_services("cargo build --release"));
        assert!(!command_needs_system_services("ls -la && npm run bundle"));
        assert!(!command_needs_system_services("git status"));
        assert!(!command_needs_system_services(""));
        // A mention inside an echo argument is not an invocation.
        assert!(!command_needs_system_services("echo 'run hdiutil later'"));
    }
}
