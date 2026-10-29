//! Guard for `brain_shell`: refuses commands a brain tool already covers.
//!
//! The guard is behavioural, not a sandbox: it inspects the command string
//! before `bash` sees it. A command that a brain tool replaces is refused with
//! the name of that tool, so the fallback to `grep`/`sed`/`cat` stops being
//! silent.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(String),
    Op(Op),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Pipe,
    And,
    Or,
    Semi,
    Background,
    Redirect,
    Append,
    RedirectAll,
    Input,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub append: bool,
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<String>,
    pub writes: Vec<Write>,
}

/// Splits a shell command into words and operators, honouring quotes and
/// backslash escapes.
pub fn tokenize(command: &str) -> Vec<Token> {
    fn flush(tokens: &mut Vec<Token>, word: &mut String, in_word: &mut bool) {
        if *in_word {
            tokens.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    }

    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => flush(&mut tokens, &mut word, &mut in_word),
            '\n' => {
                flush(&mut tokens, &mut word, &mut in_word);
                tokens.push(Token::Op(Op::Semi));
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            '#' if !in_word => {
                while let Some(c) = chars.next() {
                    if c == '\n' {
                        tokens.push(Token::Op(Op::Semi));
                        break;
                    }
                }
            }
            '|' => {
                flush(&mut tokens, &mut word, &mut in_word);
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(Token::Op(Op::Or));
                } else {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    tokens.push(Token::Op(Op::Pipe));
                }
            }
            '&' => {
                flush(&mut tokens, &mut word, &mut in_word);
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(Token::Op(Op::And));
                } else if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Op(Op::RedirectAll));
                } else {
                    tokens.push(Token::Op(Op::Background));
                }
            }
            ';' => {
                flush(&mut tokens, &mut word, &mut in_word);
                tokens.push(Token::Op(Op::Semi));
            }
            '>' => {
                // `2> file`: the digit is a file descriptor, not a word.
                if in_word && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                } else {
                    flush(&mut tokens, &mut word, &mut in_word);
                }
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Op(Op::Append));
                } else {
                    tokens.push(Token::Op(Op::Redirect));
                    // `>&1`: duplication of a descriptor, not a write to a file.
                    if chars.peek() == Some(&'&') {
                        chars.next();
                        let mut fd = String::from("&");
                        while let Some(d) = chars.peek() {
                            if !d.is_ascii_digit() {
                                break;
                            }
                            fd.push(*d);
                            chars.next();
                        }
                        if fd.len() > 1 {
                            tokens.push(Token::Word(fd));
                        }
                    }
                }
            }
            '<' => {
                flush(&mut tokens, &mut word, &mut in_word);
                tokens.push(Token::Op(Op::Input));
            }
            _ => {
                word.push(c);
                in_word = true;
            }
        }
    }
    flush(&mut tokens, &mut word, &mut in_word);
    tokens
}

/// Splits a command into pipeline segments, keeping each segment's words and
/// its file write targets (`>`, `>>`, `&>`).
pub fn segments(command: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut current = Segment::default();
    let mut pending: Option<Op> = None;

    for token in tokenize(command) {
        match token {
            Token::Word(word) => match pending.take() {
                Some(Op::Redirect) | Some(Op::RedirectAll) => {
                    if !is_fd_target(&word) {
                        current.writes.push(Write {
                            append: false,
                            target: word,
                        });
                    }
                }
                Some(Op::Append) => {
                    if !is_fd_target(&word) {
                        current.writes.push(Write {
                            append: true,
                            target: word,
                        });
                    }
                }
                Some(Op::Input) => {}
                Some(_) => unreachable!("only redirects leave a pending target"),
                None => current.words.push(word),
            },
            Token::Op(op) => match op {
                Op::Redirect | Op::Append | Op::RedirectAll | Op::Input => pending = Some(op),
                _ => {
                    if !current.words.is_empty() || !current.writes.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                    pending = None;
                }
            },
        }
    }

    if !current.words.is_empty() || !current.writes.is_empty() {
        out.push(current);
    }
    out
}

fn is_fd_target(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('&') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// A command refused by the guard: what matched and which tool replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub program: String,
    pub replacement: &'static str,
}

/// Returns the first covered command in `command`, if any.
pub fn check(command: &str) -> Option<Blocked> {
    segments(command)
        .into_iter()
        .find_map(|segment| check_segment(&segment))
}

/// The refusal text handed back to the agent.
pub fn message(blocked: &Blocked) -> String {
    format!(
        "### brain_shell — comando bloqueado\n\n\
         `{program}` é substituído por `{replacement}`.\n\n\
         Use `{replacement}` em vez do shell. Pesquisa e leitura de código são\n\
         `brain_search`, `brain_read`, `brain_map`, `brain_context_graph` e\n\
         `brain_impact_check`. `brain_shell` é só para git, formatador e ferramenta externa.",
        program = blocked.program,
        replacement = blocked.replacement,
    )
}

fn check_segment(segment: &Segment) -> Option<Blocked> {
    let mut words = segment.words.clone();

    loop {
        while words.first().is_some_and(|word| is_assignment(word)) {
            words.remove(0);
        }
        let Some(first) = words.first() else {
            break;
        };
        let program = basename(first);

        if is_wrapper(&program) {
            match program.as_str() {
                "bash" | "sh" | "zsh" | "dash" | "ksh" => {
                    return shell_script(&words).and_then(|script| check(&script));
                }
                "eval" => return check(&words[1..].join(" ")),
                "busybox" => {
                    words.remove(0);
                    continue;
                }
                "xargs" => {
                    words.remove(0);
                    skip_options(&mut words, XARGS_VALUE_OPTIONS);
                    if words.is_empty() {
                        return None;
                    }
                    continue;
                }
                "command" => {
                    if words.iter().skip(1).any(|w| w == "-v" || w == "-V") {
                        return None;
                    }
                    strip_wrapper(&mut words, &[]);
                }
                "sudo" => strip_wrapper(&mut words, SUDO_VALUE_OPTIONS),
                "env" => strip_wrapper(&mut words, ENV_VALUE_OPTIONS),
                "time" => strip_wrapper(&mut words, TIME_VALUE_OPTIONS),
                "nice" => strip_wrapper(&mut words, NICE_VALUE_OPTIONS),
                "stdbuf" => strip_wrapper(&mut words, STDBUF_VALUE_OPTIONS),
                "nohup" => strip_wrapper(&mut words, &[]),
                _ => unreachable!(),
            }
            continue;
        }

        if let Some(blocked) = classify(&program, &words) {
            return Some(blocked);
        }
        break;
    }

    for write in &segment.writes {
        if is_ignored_target(&write.target) {
            continue;
        }
        let op = if write.append { ">>" } else { ">" };
        return Some(Blocked {
            program: format!("{op} {}", write.target),
            replacement: "brain_write",
        });
    }

    None
}

fn classify(program: &str, words: &[String]) -> Option<Blocked> {
    let (replacement, label) = match program {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "ugrep" => ("brain_search", program),
        "cat" | "bat" | "less" | "more" | "most" | "head" | "tail" | "nl" | "tac" | "strings"
        | "xxd" | "od" | "hexdump" => ("brain_read", program),
        "awk" | "gawk" | "mawk" | "cut" => ("brain_read", program),
        "sed" => {
            let edit = words.iter().skip(1).any(|w| w.starts_with("-i"));
            (if edit { "brain_edit" } else { "brain_read" }, program)
        }
        "find" | "fd" | "fdfind" | "locate" | "tree" | "ls" | "dir" | "vdir" => {
            ("brain_map", program)
        }
        "tee" | "dd" | "truncate" => ("brain_write", program),
        "patch" => ("brain_patch", program),
        "git" => return git_target(words),
        "cargo" => return cargo_target(words),
        _ => return None,
    };
    Some(Blocked {
        program: label.to_string(),
        replacement,
    })
}

fn git_target(words: &[String]) -> Option<Blocked> {
    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        if word == "-C" || word == "-c" {
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }

    let subcommand = words.get(index)?;
    let (replacement, label) = match subcommand.as_str() {
        "grep" => ("brain_search", "git grep"),
        "diff" => ("brain_diff", "git diff"),
        "show" => ("brain_diff", "git show"),
        "blame" => ("brain_why", "git blame"),
        "apply" => ("brain_patch", "git apply"),
        "log" if words[index + 1..].iter().any(|w| w.starts_with("-L")) => {
            ("brain_why", "git log -L")
        }
        _ => return None,
    };
    Some(Blocked {
        program: label.to_string(),
        replacement,
    })
}

fn cargo_target(words: &[String]) -> Option<Blocked> {
    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        if word.starts_with('+') || word.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }

    let subcommand = words.get(index)?;
    if matches!(subcommand.as_str(), "build" | "check" | "test" | "clippy") {
        Some(Blocked {
            program: format!("cargo {subcommand}"),
            replacement: "brain_exec",
        })
    } else {
        None
    }
}

const SUDO_VALUE_OPTIONS: &[&str] = &["-u", "-g", "-C", "-p", "-h", "-r", "-t", "-U", "-T"];
const ENV_VALUE_OPTIONS: &[&str] = &["-u", "-C", "-S", "--unset", "--chdir"];
const TIME_VALUE_OPTIONS: &[&str] = &["-f", "-o", "--format", "--output"];
const NICE_VALUE_OPTIONS: &[&str] = &["-n", "--adjustment"];
const STDBUF_VALUE_OPTIONS: &[&str] = &[
    "-i", "-o", "-e", "--input", "--output", "--error",
];
const XARGS_VALUE_OPTIONS: &[&str] = &[
    "-n",
    "-P",
    "-I",
    "-L",
    "-s",
    "-E",
    "-d",
    "--max-args",
    "--max-procs",
    "--replace",
    "--delimiter",
];

fn is_wrapper(program: &str) -> bool {
    matches!(
        program,
        "sudo"
            | "env"
            | "time"
            | "command"
            | "nohup"
            | "nice"
            | "stdbuf"
            | "busybox"
            | "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "ksh"
            | "eval"
            | "xargs"
    )
}

fn strip_wrapper(words: &mut Vec<String>, value_options: &[&str]) {
    words.remove(0);
    skip_options(words, value_options);
}

fn skip_options(words: &mut Vec<String>, value_options: &[&str]) {
    while let Some(word) = words.first() {
        if word == "--" {
            words.remove(0);
            break;
        }
        if !word.starts_with('-') {
            break;
        }
        let takes_value = value_options.contains(&word.as_str());
        words.remove(0);
        if takes_value && !words.is_empty() {
            words.remove(0);
        }
    }
}

fn shell_script(words: &[String]) -> Option<String> {
    let args = &words[1..];
    let mut index = 0;
    while index < args.len() {
        let word = &args[index];
        if word.starts_with('-') && !word.starts_with("--") && word.contains('c') {
            return args.get(index + 1).cloned();
        }
        index += 1;
    }
    None
}

fn basename(word: &str) -> String {
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
        .to_string()
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.contains('/')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_ignored_target(target: &str) -> bool {
    matches!(target, "/dev/null" | "/dev/stdout" | "/dev/stderr") || is_fd_target(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pipelines_and_keeps_quotes_together() {
        assert_eq!(
            tokenize("git log | grep 'fix bug' && echo ok"),
            vec![
                Token::Word("git".into()),
                Token::Word("log".into()),
                Token::Op(Op::Pipe),
                Token::Word("grep".into()),
                Token::Word("fix bug".into()),
                Token::Op(Op::And),
                Token::Word("echo".into()),
                Token::Word("ok".into()),
            ]
        );
    }

    #[test]
    fn keeps_fd_duplication_out_of_write_targets() {
        let segs = segments("cargo test 2>&1 | tail -5");
        assert_eq!(segs.len(), 2);
        assert!(segs[0].writes.is_empty());
        assert_eq!(segs[1].words, vec!["tail", "-5"]);
    }

    #[test]
    fn records_file_redirects_as_writes() {
        let segs = segments("echo hi > out.txt");
        assert_eq!(segs[0].writes.len(), 1);
        assert_eq!(segs[0].writes[0].target, "out.txt");
        assert!(!segs[0].writes[0].append);
        assert_eq!(segs[0].words, vec!["echo", "hi"]);
    }

    #[test]
    fn a_digit_before_redirect_is_an_fd_not_a_word() {
        let segs = segments("cmd 2> err.log");
        assert_eq!(segs[0].words, vec!["cmd"]);
        assert_eq!(segs[0].writes.len(), 1);
        assert_eq!(segs[0].writes[0].target, "err.log");
        assert!(!segs[0].writes[0].append);
    }

    #[test]
    fn a_backslash_escapes_the_next_character() {
        assert_eq!(
            tokenize(r"echo a\ b"),
            vec![Token::Word("echo".into()), Token::Word("a b".into())]
        );
    }

    fn blocked(command: &str) -> Option<(String, &'static str)> {
        check(command).map(|b| (b.program, b.replacement))
    }

    #[test]
    fn blocks_search_programs_anywhere_in_the_pipeline() {
        assert_eq!(
            blocked("git log | grep fix"),
            Some(("grep".into(), "brain_search"))
        );
        assert_eq!(blocked("rg TODO src"), Some(("rg".into(), "brain_search")));
        assert_eq!(
            blocked("sed -n '1,20p' src/lib.rs"),
            Some(("sed".into(), "brain_read"))
        );
        assert_eq!(
            blocked("sed -i 's/a/b/' src/lib.rs"),
            Some(("sed".into(), "brain_edit"))
        );
        assert_eq!(
            blocked("find . -name '*.rs'"),
            Some(("find".into(), "brain_map"))
        );
    }

    #[test]
    fn unwraps_wrappers_and_nested_shells() {
        assert_eq!(
            blocked("sudo env FOO=1 bash -c 'rg token'"),
            Some(("rg".into(), "brain_search"))
        );
        assert_eq!(
            blocked("printf x | xargs grep foo"),
            Some(("grep".into(), "brain_search"))
        );
        assert_eq!(
            blocked("eval \"cat Cargo.toml\""),
            Some(("cat".into(), "brain_read"))
        );
        assert_eq!(
            blocked("/usr/bin/grep needle Cargo.toml"),
            Some(("grep".into(), "brain_search"))
        );
        assert_eq!(
            blocked("\"grep\" needle Cargo.toml"),
            Some(("grep".into(), "brain_search"))
        );
    }

    #[test]
    fn reads_git_and_cargo_subcommands_past_global_options() {
        assert_eq!(
            blocked("git -C sub diff --stat"),
            Some(("git diff".into(), "brain_diff"))
        );
        assert_eq!(
            blocked("git log -L 1,20:src/a.rs"),
            Some(("git log -L".into(), "brain_why"))
        );
        assert_eq!(
            blocked("cargo +nightly test --all"),
            Some(("cargo test".into(), "brain_exec"))
        );
        assert_eq!(
            blocked("cargo clippy -- -D warnings"),
            Some(("cargo clippy".into(), "brain_exec"))
        );
    }

    #[test]
    fn blocks_file_writes_but_not_fd_duplication_or_dev_null() {
        assert_eq!(
            blocked("echo hi > out.txt"),
            Some(("> out.txt".into(), "brain_write"))
        );
        assert_eq!(
            blocked("echo hi >> out.txt"),
            Some((">> out.txt".into(), "brain_write"))
        );
        assert_eq!(blocked("echo hi 2>&1 > /dev/null"), None);
    }

    #[test]
    fn leaves_uncovered_commands_alone() {
        assert_eq!(blocked("git status"), None);
        assert_eq!(blocked("git commit -m 'x'"), None);
        assert_eq!(blocked("cargo fmt"), None);
        assert_eq!(blocked("mkdir -p tmp"), None);
        assert_eq!(blocked("make build"), None);
        assert_eq!(blocked("echo hello"), None);
    }

    #[test]
    fn the_message_names_the_program_and_the_replacement() {
        let text = message(&check("grep x").unwrap());
        assert!(text.contains("`grep`"));
        assert!(text.contains("brain_search"));
        assert!(text.contains("brain_shell"));
    }
}
