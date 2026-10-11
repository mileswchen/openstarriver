//! Pure Windows-backend logic: deterministic synthetic SID derivation, MSVC
//! command-line encoding, and the helper argv template. No Win32 calls, so
//! every function is unit-testable where the module compiles.

use starriver_kernel::sandbox::SandboxMode;

/// FNV-1a 64-bit — hand-rolled (not `DefaultHasher`) because the SID it
/// derives must stay byte-stable across rustc releases: a changed hash would
/// orphan previously granted ACEs in workspace DACLs.
pub fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Deterministic synthetic SID for one (tag, unique) pair, shaped
/// `S-1-5-21-{a}-{b}-{c}`. Every subauthority has bit 30 forced on: real
/// account RIDs are small, so a hash-derived value can never collide with an
/// actual principal of a machine domain (which would silently grant writes
/// to it). The SID is derived, never persisted and never random — the same
/// workspace always regenerates the same SID, which is what makes the grant
/// ACEs idempotent across sessions without a state file.
pub fn sid_string(tag: &str, unique: &str) -> String {
    let hash = fnv1a64(format!("{tag}:{unique}").as_bytes());
    let a = ((hash) as u32) | 0x4000_0000;
    let b = ((hash >> 21) as u32) | 0x4000_0000;
    let c = ((hash >> 42) as u32) | 0x4000_0000;
    format!("S-1-5-21-{a}-{b}-{c}")
}

/// ReadOnly restricting SID. Never granted anywhere by construction (nothing
/// but this derivation can produce it), so the write check against it fails
/// on every object — full write denial with zero ACL mutation.
pub fn readonly_sid() -> String {
    sid_string("starriver-sbx-readonly", "v1")
}

/// WorkspaceWrite restricting SID for one workspace root. Grant ACEs for it
/// are applied to the workspace root + %TEMP% before each confined spawn.
pub fn workspace_sid(workspace_root: &str) -> String {
    sid_string("starriver-sbx-ws", workspace_root)
}

/// MSVC command-line encoding (the standard `argv → one string` quoting:
/// backslash runs before `"` are doubled plus an escape, trailing runs are
/// doubled). Literal newlines survive inside quotes — CommandLineToArgvW and
/// the MSYS runtime only split on space/tab — so multi-line `sh -c` bodies
/// pass through intact.
pub fn encode_cmdline(args: &[String]) -> String {
    args.iter()
        .map(|arg| encode_one(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn encode_one(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '"' | '\n' | '\r'))
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

/// Helper argv template for one confined run (parent side):
/// `starriver-sbx-helper run <mode> <root> -- <child argv…>`. `root` is
/// passed through verbatim — canonicalization (and the `\\?\` stripping the
/// Win32 ACL APIs dislike) is the caller's job.
pub fn helper_argv(mode: SandboxMode, workspace_root: &str, helper: &str) -> Vec<String> {
    vec![
        helper.to_string(),
        "run".into(),
        mode.as_str().into(),
        workspace_root.into(),
        "--".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn derived_sids_are_stable_and_well_formed() {
        let ws = workspace_sid(r"C:\work\proj");
        assert_eq!(ws, workspace_sid(r"C:\work\proj"), "same root → same SID");
        assert_ne!(
            ws,
            workspace_sid(r"C:\work\other"),
            "different roots differ"
        );
        assert_ne!(ws, readonly_sid());
        for sid in [ws, readonly_sid()] {
            let rest = sid.strip_prefix("S-1-5-21-").expect("domain-shaped SID");
            let subs: Vec<&str> = rest.split('-').collect();
            assert_eq!(subs.len(), 3, "{sid}");
            for s in subs {
                let v: u32 = s.parse().expect("decimal subauthority");
                assert_ne!(v & 0x4000_0000, 0, "bit-30 guard set: {sid}");
            }
        }
    }

    #[test]
    fn plain_args_stay_unquoted() {
        assert_eq!(encode_cmdline(&["cmd".into(), "/c".into()]), "cmd /c");
    }

    #[test]
    fn quoting_round_trips_spaces_and_quotes() {
        let args = vec![
            "sh".into(),
            "-c".into(),
            "echo \"a b\" > C:\\temp dir\\out.txt".into(),
        ];
        let encoded = encode_cmdline(&args);
        // Backslashes NOT adjacent to a quote stay single (only runs
        // immediately before a `"` double up).
        assert_eq!(
            encoded,
            "sh -c \"echo \\\"a b\\\" > C:\\temp dir\\out.txt\""
        );
        // Multiline bodies keep their newlines (split is space/tab only).
        let multiline = vec!["-c".into(), "echo a\necho b".into()];
        assert!(encode_cmdline(&multiline).contains("echo a\necho b"));
    }

    #[test]
    fn backslash_runs_double_only_before_quotes() {
        // Bare backslashes without whitespace/quotes need no quoting at all
        // (CommandLineToArgvW treats them as literals).
        assert_eq!(encode_cmdline(&["a\\".into()]), "a\\");
        // A trailing run right before the closing quote doubles.
        let encoded = encode_cmdline(&["a \\".into()]);
        assert_eq!(encoded, "\"a \\\\\"");
        // …and so does a run directly preceding an embedded quote: the run
        // `a \"b` (one backslash before the `"`) doubles to `\\` plus the
        // escaping `\` — three backslashes — so CommandLineToArgvW decodes
        // the argument back to `a \"b`.
        let encoded = encode_cmdline(&["a \\\"b".into()]);
        assert_eq!(encoded, "\"a \\\\\\\"b\"");
    }

    #[test]
    fn helper_argv_carries_mode_root_and_separator() {
        let argv = helper_argv(SandboxMode::WorkspaceWrite, r"C:\ws", r"C:\bin\sbx.exe");
        assert_eq!(
            argv,
            vec![
                r"C:\bin\sbx.exe".to_string(),
                "run".into(),
                "workspace-write".into(),
                r"C:\ws".into(),
                "--".into(),
            ]
        );
        let argv = helper_argv(SandboxMode::ReadOnly, r"C:\ws", "h");
        assert_eq!(argv[2], "read-only");
    }
}
