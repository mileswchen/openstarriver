//! WI-12: fine-grained bash command classification for the read-only
//! sandbox. `bash` itself is `deny_in_readonly: false` because most of its
//! uses are pure reads — so the decision is made per command line: every
//! pipeline/sequence segment must resolve to a read-only allowlisted
//! command, with no redirections, command substitution, or side-effecting
//! flags. Fail-closed: anything not recognized as read-only is treated as
//! potentially writing.

/// Commands allowed as-is in the read-only sandbox (first word of a
/// segment, after env-var assignments). Conservative core of pure readers.
const READONLY_COMMANDS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "ls",
    "pwd",
    "echo",
    "printf",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "wc",
    "cut",
    "diff",
    "cmp",
    "comm",
    "file",
    "stat",
    "du",
    "df",
    "ps",
    "printenv",
    "which",
    "whereis",
    "whoami",
    "id",
    "date",
    "uname",
    "hostname",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "cksum",
    "jq",
    "tr",
    "test",
    "[",
    "true",
    "false",
    "sleep",
];

/// `git` subcommands that are pure reads. Ambiguous ones (`branch`, `tag`,
/// `config`, `remote`, `worktree` — writes depend on flags/args) and all
/// mutating subcommands are treated as writes.
const GIT_READONLY_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "rev-parse",
    "describe",
    "ls-files",
    "ls-remote",
    "shortlog",
    "reflog",
    "cat-file",
    "grep",
    "help",
    "version",
];

/// `find` flags with side effects — any of them disqualifies the command.
const FIND_WRITE_FLAGS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprintf", "-fls",
];

/// True when every segment of the command line is a read-only allowlisted
/// command. An empty or unparseable command is `false` (fail-closed).
pub fn bash_command_is_readonly(command: &str) -> bool {
    if command.trim().is_empty() {
        return false; // fail-closed: nothing to classify
    }
    // Quote-aware scan for write-ish constructs anywhere in the line.
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in chars.iter().copied().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            // Backquote and $(...) expand INSIDE double quotes — only
            // single quotes shield them (BUG-94); `>` stays literal in
            // double quotes (existing pinned semantics).
            '>' if !in_single && !in_double => return false,
            '`' | '\n' | '\r' if !in_single => return false,
            '<' if !in_single && chars.get(i + 1) == Some(&'(') => return false,
            '$' if !in_single && chars.get(i + 1) == Some(&'(') => return false,
            _ => {}
        }
    }
    if in_single || in_double {
        return false; // unbalanced quotes — fail-closed
    }
    split_segments(command)
        .into_iter()
        .all(|segment| segment_is_readonly(&segment))
}

/// Split on shell control operators (`&&`, `||`, `;`, `|`, `&`, parens)
/// outside quotes.
pub(crate) fn split_segments(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            ';' | '|' | '&' | '(' | ')' | '\n' | '\r' if !in_single && !in_double => {
                if c == '&' && chars.get(i + 1) == Some(&'&') {
                    i += 1; // consume the second '&' of `&&`
                }
                if c == '|' && chars.get(i + 1) == Some(&'|') {
                    i += 1; // consume the second '|' of `||`
                }
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
        i += 1;
    }
    segments.push(current);
    segments
}

/// Tokenize one segment on whitespace, honoring quotes.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in segment.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            c if !in_single && !in_double && c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_env_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    let name = &token[..eq];
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !token.starts_with('-')
}

fn segment_is_readonly(segment: &str) -> bool {
    let tokens = tokenize(segment);
    if tokens.is_empty() {
        return true; // nothing runs in this segment
    }
    // Skip leading env-var assignments (FOO=bar cmd …).
    let mut idx = 0;
    while idx < tokens.len() && is_env_assignment(&tokens[idx]) {
        idx += 1;
    }
    if idx >= tokens.len() {
        return true; // only assignments, no command
    }
    // Strip quotes from EVERY token before inspection: `'-o'` and
    // `"-delete"` are the same flags as their unquoted forms (the shell
    // removes the quotes), but the raw-token comparison let a quoted write
    // flag slip past the scan.
    let tokens: Vec<String> = tokens
        .iter()
        .map(|t| t.trim_matches('\'').trim_matches('"').to_string())
        .collect();
    let raw = &tokens[idx];
    let program = raw.rsplit('/').next().unwrap_or(raw);
    // Privilege escalation and interpreters are never read-only.
    if matches!(
        program,
        "sudo"
            | "su"
            | "doas"
            | "sh"
            | "bash"
            | "zsh"
            | "python"
            | "python3"
            | "perl"
            | "ruby"
            | "node"
            | "awk"
            | "sed"
            | "xargs"
            | "tee"
    ) {
        return false;
    }
    match program {
        "git" => tokens
            .get(idx + 1)
            .map(|sub| {
                let sub = sub.trim_matches('\'').trim_matches('"');
                GIT_READONLY_SUBCOMMANDS.contains(&sub)
            })
            .unwrap_or(false),
        "find" => !tokens[idx..]
            .iter()
            .any(|t| FIND_WRITE_FLAGS.contains(&t.as_str())),
        "sort" => !tokens[idx..]
            .iter()
            .any(|t| t == "-o" || t.starts_with("--output")),
        _ => READONLY_COMMANDS.contains(&program),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_reads_are_readonly() {
        for command in [
            "ls -la",
            "cat foo.txt",
            "grep -rn pattern src",
            "rg --files",
            "pwd",
            "echo hi",
            "wc -l notes.md",
            "stat Cargo.toml",
            "cat a | grep b | wc -l",
            "ls; pwd",
            "cat a && grep x a || echo missing",
            "FOO=1 ls",
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1",
            "find . -name '*.rs'",
            "sort items.txt",
        ] {
            assert!(
                bash_command_is_readonly(command),
                "should be readonly: {command}"
            );
        }
    }

    #[test]
    fn writing_commands_are_refused() {
        for command in [
            "rm -rf /",
            "touch new.txt",
            "mkdir build",
            "cp a b",
            "mv a b",
            "chmod +x run.sh",
            "git commit -m x",
            "git push",
            "git config user.email x@y",
            "git branch -D feature",
            "sed -i 's/a/b/' f.txt",
            "awk '{print $1}' f.txt",
            "python solve.py",
            "sudo cat /etc/shadow",
            "echo hi > out.txt",
            "cat in >> log",
            "echo $(date)",
            "echo `date`",
            "find . -name x -delete",
            "find . -exec rm {} ;",
            "sort -o out in",
            "tee out.txt",
            "",
        ] {
            assert!(
                !bash_command_is_readonly(command),
                "should be refused: {command}"
            );
        }
    }

    #[test]
    fn quoting_shields_literal_operators() {
        // A '>' inside quotes is a literal character, not redirection.
        assert!(bash_command_is_readonly("grep 'a>b' file.txt"));
        assert!(bash_command_is_readonly("echo \"not > a redirect\""));
    }
}


// ── Round-6 regressions (BUG-93 / BUG-94 / find -fls) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn newline_is_a_segment_separator() {
        assert!(
            !bash_command_is_readonly("cat notes.md\nrm -rf /"),
            "a second command after a newline must not ride the read-only verdict"
        );
    }

    #[test]
    fn command_substitution_inside_double_quotes_is_not_readonly() {
        assert!(
            !bash_command_is_readonly("echo \"$(touch /tmp/pwn)\""),
            "$( ) expands inside double quotes"
        );
        assert!(
            !bash_command_is_readonly("echo \"`touch /tmp/pwn`\""),
            "backquotes expand inside double quotes"
        );
    }

    #[test]
    fn find_fls_flag_is_a_write_flag() {
        assert!(!bash_command_is_readonly("find . -fls /tmp/out"));
    }
}
