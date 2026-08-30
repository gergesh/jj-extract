//! Which shell commands count as an edit.
//!
//! A Bash tool call is normally invisible to recording: whatever it leaves on
//! disk is flushed to an unattributed evolution, because a shell command can do
//! anything, and folding "anything" into an agent's change is how a formatter's
//! sweep or a peer's work ends up inside it.
//!
//! But agents write files through the shell constantly — a `cat > file <<'EOF'`
//! heredoc, a `python3 - <<'PY'` script that rewrites a few lines — and those
//! edits are as much that agent's work as any `Edit` tool call. They were the
//! one hole in attribution.
//!
//! So a command is treated as an edit when *every* part of it is a write we
//! recognize or is harmless, and at least one part writes. Anything else — a
//! build, a formatter, `git`, a command not on the list — stays unattributed.
//! The conservatism is deliberate and one-directional: missing an edit only
//! leaves those lines in the live change, while wrongly claiming one silently
//! folds somebody else's work into the agent's.

use std::path::Path;

/// Commands that change nothing on their own: whatever they write, they write
/// through a redirection we can see. `tee` and `touch` name their targets as
/// arguments instead, and `mkdir` only makes directories, which jj doesn't
/// track.
const HARMLESS: &[&str] = &[
    "basename", "cat", "cd", "cut", "date", "dirname", "echo", "grep", "head", "ls", "mkdir",
    "printf", "pwd", "sed", "sort", "tail", "tee", "test", "touch", "tr", "true", "uniq", "wc",
];

/// Interpreters, accepted only when the script is inline (a heredoc or `-c`).
/// A script read from a file is a program, not an edit typed into the shell.
const INTERPRETERS: &[&str] = &["node", "perl", "python", "python3", "ruby"];

/// Commands whose arguments are the files they write.
const NAMES_ITS_TARGETS: &[&str] = &["tee", "touch"];

/// Marker left where a heredoc's `<<TAG` operator was, so the lexer can still
/// see that a segment was fed an inline script after the body is removed.
const HEREDOC: &str = "\u{1}heredoc";

#[derive(Default)]
struct Segment {
    words: Vec<String>,
    /// One entry per redirection that writes, holding the target path when it
    /// is literal enough to name (no expansion, no glob).
    writes: Vec<Option<String>>,
    heredoc: bool,
}

/// The paths a command writes, or None when the command must stay
/// unattributed. `Some(vec![])` is a real answer: a `python3 - <<'PY'` script
/// edits files it never names, and its changes are still the agent's.
pub fn written_paths(command: &str) -> Option<Vec<String>> {
    let script = strip_heredocs(command)?;
    let segments = lex(&script)?;
    let mut targets = Vec::new();
    let mut writes = false;
    let mut relative_targets_are_unknown = false;
    for segment in &segments {
        let effect = segment_effect(segment)?;
        writes |= effect.writes || !segment.writes.is_empty();
        // A `cd` moves what a later relative path means, and the hook only
        // knows the directory the tool started in.
        relative_targets_are_unknown |= segment.words.first().is_some_and(|word| word == "cd");
        targets.extend(segment.writes.iter().flatten().cloned());
        targets.extend(effect.targets);
    }
    if !writes {
        return None;
    }
    if relative_targets_are_unknown {
        targets.retain(|target| Path::new(target).is_absolute());
    }
    Some(targets)
}

/// What running one segment does to the working copy.
#[derive(Default)]
struct Effect {
    /// Whether it writes files of its own, beyond any redirection.
    writes: bool,
    /// The literal targets it names itself.
    targets: Vec<String>,
}

/// `segment`'s effect, or None when it is not something we will attribute.
fn segment_effect(segment: &Segment) -> Option<Effect> {
    let Some(command) = segment.words.first() else {
        // Only redirections, e.g. `> file`, which truncates it.
        return Some(Effect::default());
    };
    if NAMES_ITS_TARGETS.contains(&command.as_str()) {
        return Some(Effect {
            writes: true,
            targets: segment
                .words
                .iter()
                .skip(1)
                .filter(|word| !word.starts_with('-'))
                .cloned()
                .collect(),
        });
    }
    if HARMLESS.contains(&command.as_str()) {
        return Some(Effect::default());
    }
    // An inline script writes files it never names; the snapshot still catches
    // every one of them that jj already tracks.
    if INTERPRETERS.contains(&command.as_str())
        && (segment.heredoc
            || segment
                .words
                .iter()
                .any(|word| word == "-c" || word == "-e"))
    {
        return Some(Effect {
            writes: true,
            targets: Vec::new(),
        });
    }
    None
}

/// Remove every heredoc body, leaving a marker where its operator was. The
/// bodies are data — they can hold anything, including text that would lex as
/// shell — so they must not reach the lexer.
fn strip_heredocs(command: &str) -> Option<String> {
    let mut out = String::new();
    let mut lines = command.lines();
    while let Some(line) = lines.next() {
        let mut rest = line;
        let mut tags = Vec::new();
        while let Some(at) = rest.find("<<") {
            let (before, operator) = rest.split_at(at);
            out.push_str(before);
            let after = operator[2..].trim_start_matches('-');
            // `<<<` is a here-string: its word is on this line, nothing to strip.
            if after.starts_with('<') {
                out.push_str("<<");
                rest = &operator[2..];
                continue;
            }
            let after = after.trim_start();
            let (tag, remainder) = heredoc_tag(after)?;
            tags.push(tag);
            out.push(' ');
            out.push_str(HEREDOC);
            out.push(' ');
            rest = remainder;
        }
        out.push_str(rest);
        out.push('\n');
        for tag in tags {
            let mut terminated = false;
            for body in lines.by_ref() {
                if body.trim() == tag {
                    terminated = true;
                    break;
                }
            }
            if !terminated {
                return None; // an unterminated heredoc: not a command we can read
            }
        }
    }
    Some(out)
}

/// The delimiter of a heredoc starting at `after`, and what follows it.
fn heredoc_tag(after: &str) -> Option<(String, &str)> {
    let mut chars = after.char_indices();
    let (_, first) = chars.next()?;
    if first == '\'' || first == '"' {
        let end = after[1..].find(first)? + 1;
        return Some((after[1..end].to_string(), &after[end + 1..]));
    }
    let end = after
        .find(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    Some((after[..end].to_string(), &after[end..]))
}

/// Split a heredoc-free script into segments, refusing anything whose effects
/// can't be read off the text: command substitution, subshells, backgrounding.
// The `finish_word!()` after the loop resets state nothing reads again.
#[allow(unused_assignments)]
fn lex(script: &str) -> Option<Vec<Segment>> {
    let mut segments = vec![Segment::default()];
    let mut word = String::new();
    let mut started = false;
    let mut literal = true;
    let mut redirect = false;
    let mut swallow_input = false;
    let mut chars = script.chars().peekable();

    macro_rules! finish_word {
        () => {
            if started {
                let text = std::mem::take(&mut word);
                let segment = segments.last_mut()?;
                if swallow_input {
                    swallow_input = false;
                } else if redirect {
                    segment.writes.push(literal.then_some(text));
                    redirect = false;
                } else if text == HEREDOC {
                    segment.heredoc = true;
                } else {
                    segment.words.push(text);
                }
                started = false;
                literal = true;
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => {
                            if c == '$' || c == '`' {
                                literal = false;
                            }
                            word.push(c);
                        }
                    }
                }
            }
            '\\' => {
                started = true;
                word.push(chars.next()?);
            }
            '`' | '(' | ')' | '{' | '}' => return None,
            '$' => {
                // `$(...)` is a subshell; a bare `$name` only makes the word
                // unusable as a path.
                if chars.peek() == Some(&'(') {
                    return None;
                }
                started = true;
                literal = false;
                word.push(c);
            }
            '*' | '?' | '[' => {
                started = true;
                literal = false;
                word.push(c);
            }
            '#' if !started => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                finish_word!();
                segments.push(Segment::default());
            }
            ' ' | '\t' => finish_word!(),
            ';' | '\n' => {
                finish_word!();
                segments.push(Segment::default());
            }
            '|' => {
                finish_word!();
                chars.next_if_eq(&'|');
                segments.push(Segment::default());
            }
            '&' => {
                finish_word!();
                // `&&` chains; a lone `&` backgrounds the command, which would
                // still be running when PostToolUse snapshots.
                chars.next_if_eq(&'&')?;
                segments.push(Segment::default());
            }
            '<' => {
                finish_word!();
                chars.next_if_eq(&'<');
                swallow_input = true;
            }
            '>' => {
                // A leading file descriptor (`2>`) is part of the operator.
                if started && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    started = false;
                }
                finish_word!();
                chars.next_if_eq(&'>');
                if chars.next_if_eq(&'&').is_some() {
                    // `>&2` duplicates a descriptor; it creates no file.
                    while chars.next_if(|c| c.is_ascii_digit() || *c == '-').is_some() {}
                } else {
                    redirect = true;
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    finish_word!();
    if redirect {
        return None; // a redirect with no target: not a command we can read
    }
    Some(segments)
}

#[cfg(test)]
mod tests {
    use super::written_paths;

    #[test]
    fn attributes_a_heredoc_write_and_names_its_file() {
        let paths =
            written_paths("cat > src/new.rs <<'RUST'\nfn main() {}\n> not a redirect\nRUST");
        assert_eq!(
            paths.as_deref(),
            Some(["src/new.rs".to_string()].as_slice())
        );
    }

    #[test]
    fn attributes_an_inline_script_that_names_no_file() {
        let paths = written_paths("python3 - <<'PY'\nopen('x','w').write('hi')\nPY");
        assert_eq!(paths, Some(vec![]));
    }

    #[test]
    fn attributes_a_chain_of_writes_and_harmless_commands() {
        let paths = written_paths("mkdir -p docs && printf 'hi\\n' > docs/a.md; tee docs/b.md");
        assert_eq!(
            paths,
            Some(vec!["docs/a.md".to_string(), "docs/b.md".to_string()])
        );
    }

    #[test]
    fn refuses_commands_that_do_more_than_write_files() {
        for command in [
            "cargo fmt",
            "printf 'x' > a.txt && git add a.txt",
            "rm -f a.txt",
            "python3 script.py",
            "sh -c 'printf x > a.txt'",
        ] {
            assert_eq!(written_paths(command), None, "{command}");
        }
    }

    #[test]
    fn refuses_effects_it_cannot_read_off_the_text() {
        for command in [
            "cat > $(mktemp)",
            "cat > `mktemp`",
            "printf 'x' > a.txt &",
            "cat > a.txt <<'EOF'\nunterminated",
            "cat >",
        ] {
            assert_eq!(written_paths(command), None, "{command}");
        }
    }

    #[test]
    fn ignores_reads_and_targets_it_cannot_name() {
        assert_eq!(written_paths("grep -rn todo src/"), None);
        assert_eq!(written_paths("cat src/main.rs"), None);
        assert_eq!(written_paths("printf 'x' > \"$OUT\""), Some(vec![]));
        assert_eq!(written_paths("printf 'x' > out-*.txt"), Some(vec![]));
    }

    #[test]
    fn drops_relative_targets_after_a_directory_change() {
        assert_eq!(
            written_paths("cd /tmp && printf 'x' > a.txt > /tmp/b.txt"),
            Some(vec!["/tmp/b.txt".to_string()])
        );
    }

    #[test]
    fn reads_past_redirections_that_create_no_file() {
        assert_eq!(
            written_paths("printf 'x' > a.txt 2>&1"),
            Some(vec!["a.txt".to_string()])
        );
        // A descriptor duplication writes to the terminal, not the repository.
        assert_eq!(written_paths("cat log.txt >&2"), None);
    }
}
