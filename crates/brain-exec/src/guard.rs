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
    Heredoc,
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
    /// True when the segment is fed by a heredoc instead of a file.
    pub heredoc: bool,
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

    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut in_word = false;

    while index < chars.len() {
        let c = chars[index];
        index += 1;
        match c {
            ' ' | '\t' | '\r' => flush(&mut tokens, &mut word, &mut in_word),
            '\n' => {
                flush(&mut tokens, &mut word, &mut in_word);
                tokens.push(Token::Op(Op::Semi));
            }
            '\'' => {
                in_word = true;
                while index < chars.len() && chars[index] != '\'' {
                    word.push(chars[index]);
                    index += 1;
                }
                if index < chars.len() {
                    index += 1;
                }
            }
            '"' => {
                in_word = true;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' && index + 1 < chars.len() {
                        index += 1;
                    }
                    word.push(chars[index]);
                    index += 1;
                }
                if index < chars.len() {
                    index += 1;
                }
            }
            '\\' => {
                in_word = true;
                if index < chars.len() {
                    word.push(chars[index]);
                    index += 1;
                }
            }
            '#' if !in_word => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '|' => {
                flush(&mut tokens, &mut word, &mut in_word);
                if chars.get(index) == Some(&'|') {
                    index += 1;
                    tokens.push(Token::Op(Op::Or));
                } else {
                    if chars.get(index) == Some(&'&') {
                        index += 1;
                    }
                    tokens.push(Token::Op(Op::Pipe));
                }
            }
            '&' => {
                flush(&mut tokens, &mut word, &mut in_word);
                if chars.get(index) == Some(&'&') {
                    index += 1;
                    tokens.push(Token::Op(Op::And));
                } else if chars.get(index) == Some(&'>') {
                    index += 1;
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
                if in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                } else {
                    flush(&mut tokens, &mut word, &mut in_word);
                }
                if chars.get(index) == Some(&'>') {
                    index += 1;
                    tokens.push(Token::Op(Op::Append));
                } else {
                    tokens.push(Token::Op(Op::Redirect));
                    // `>&1`: duplication of a descriptor, not a write to a file.
                    if chars.get(index) == Some(&'&') {
                        index += 1;
                        let mut fd = String::from("&");
                        while let Some(d) = chars.get(index) {
                            if !d.is_ascii_digit() {
                                break;
                            }
                            fd.push(*d);
                            index += 1;
                        }
                        if fd.len() > 1 {
                            tokens.push(Token::Word(fd));
                        }
                    }
                }
            }
            '<' => {
                flush(&mut tokens, &mut word, &mut in_word);
                if chars.get(index) == Some(&'<') {
                    index += 1;
                    // A heredoc strips leading tabs when written as <<-.
                    if chars.get(index) == Some(&'-') {
                        index += 1;
                    }
                    tokens.push(Token::Op(Op::Heredoc));
                } else {
                    tokens.push(Token::Op(Op::Input));
                }
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
                    Some(Op::Input) | Some(Op::Heredoc) => {}
                    Some(_) => unreachable!("only redirects leave a pending target"),
                    None => current.words.push(word),
                },
                Token::Op(op) => {
                    if matches!(op, Op::Heredoc) {
                        current.heredoc = true;
                    }
                    match op {
                        Op::Redirect | Op::Append | Op::RedirectAll | Op::Input | Op::Heredoc => {
                            pending = Some(op)
                        }
                        _ => {
                            if !current.words.is_empty() || !current.writes.is_empty() {
                                out.push(std::mem::take(&mut current));
                            }
                            pending = None;
                        }
                    }
                }
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
    /// Optional sentence appended to the guard message.
    pub detail: Option<&'static str>,
}

/// Returns the first covered command in `command`, if any.
pub fn check(command: &str) -> Option<Blocked> {
        fn delimiter_of(line: &str) -> Option<String> {
            let tokens = tokenize(line);
            tokens.windows(2).find_map(|pair| match (&pair[0], &pair[1]) {
                (Token::Op(Op::Heredoc), Token::Word(word)) => Some(word.clone()),
                _ => None,
            })
        }

        // O corpo de um heredoc é dado, não comando: sem isso, a linha "msg" de
        // `git commit -F - <<EOF` vira um programa e a guarda nega dado do usuário.
        fn without_heredoc_bodies(command: &str) -> String {
            let mut kept: Vec<&str> = Vec::new();
            let mut terminator: Option<String> = None;
            for line in command.split_inclusive('\n') {
                match &terminator {
                    Some(delimiter) => {
                        let stripped = line.trim_end_matches('\n').trim_end_matches('\r');
                        if stripped.trim_start_matches('\t') == delimiter {
                            terminator = None;
                        }
                    }
                    None => {
                        if let Some(delimiter) = delimiter_of(line) {
                            terminator = Some(delimiter);
                        }
                        kept.push(line);
                    }
                }
            }
            kept.concat()
        }

        let command = without_heredoc_bodies(command);
        segments(&command)
            .into_iter()
            .find_map(|segment| check_segment(&segment))
    }

/// The refusal text handed back to the agent.
pub fn message(blocked: &Blocked) -> String {
        let mut text = if blocked.replacement.is_empty() {
            format!(
                "### brain_shell — comando bloqueado\n\n\
                 `{program}` não passa pela guarda.",
                program = blocked.program,
            )
        } else {
            format!(
                "### brain_shell — comando bloqueado\n\n\
                 `{program}` é substituído por `{replacement}`.\n\n\
                 Use `{replacement}` em vez do shell. Pesquisa e leitura de código são\n\
                 `brain_search`, `brain_read`, `brain_map`, `brain_context_graph` e\n\
                 `brain_impact_check`. `brain_shell` é só para git e formatador.",
                program = blocked.program,
                replacement = blocked.replacement,
            )
        };
        if let Some(detail) = blocked.detail {
            text.push_str("\n\n");
            text.push_str(detail);
        }
        text
    }

fn check_segment(segment: &Segment) -> Option<Blocked> {
        const SHELL_DETAIL: &str = "Shell não é permitido, com ou sem script: leia com brain_read ou brain_search, altere com brain_edit, brain_write ou brain_patch e rode a suíte com brain_exec.";
        let mut words = segment.words.clone();

        loop {
            while words.first().is_some_and(|word| is_assignment(word)) {
                words.remove(0);
            }
            let Some(first) = words.first() else {
                break;
            };
            let program = basename(first);
            let shell = matches!(program.as_str(), "bash" | "sh" | "zsh" | "dash" | "ksh");

            // A heredoc feeds inline code to the program: bash and python
            // reading one are scripts, not external tools.
            if segment.heredoc && (is_interpreter(&program) || shell) {
                return Some(Blocked {
                    program: format!("{program} <<"),
                    replacement: "brain_edit",
                    detail: Some("Script passado por heredoc não passa pela guarda: para ler, use brain_read; para alterar, brain_edit, brain_write ou brain_patch; para rodar a suíte, use brain_exec."),
                });
            }

            if is_wrapper(&program) {
                match program.as_str() {
                    "bash" | "sh" | "zsh" | "dash" | "ksh" => {
                        if let Some(script) = shell_script(&words) {
                            if let Some(blocked) = check(&script) {
                                return Some(blocked);
                            }
                        }
                        return Some(Blocked {
                            program: program.clone(),
                            replacement: "",
                            detail: Some(SHELL_DETAIL),
                        });
                    }
                    "eval" => {
                        if let Some(blocked) = check(&words[1..].join(" ")) {
                            return Some(blocked);
                        }
                        return Some(Blocked {
                            program: "eval".into(),
                            replacement: "",
                            detail: Some(SHELL_DETAIL),
                        });
                    }
                    "busybox" => {
                        words.remove(0);
                        continue;
                    }
                    "xargs" => {
                        words.remove(0);
                        skip_options(&mut words, XARGS_VALUE_OPTIONS);
                        if words.is_empty() {
                            break;
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
                detail: None,
            });
        }

        None
    }

fn classify(program: &str, words: &[String]) -> Option<Blocked> {
        const OUTSIDE_GUARD: &str = "O brain_shell só roda git e formatador. Para ler, use brain_read, brain_search ou brain_map; para alterar, brain_edit, brain_write, brain_write_many, brain_patch ou brain_move; para compilar e testar, brain_exec.";
        if is_interpreter(program) {
            return interpreter_target(program, words);
        }
        // O formatador é a única ferramenta externa que sobra: ele não lê nem
        // escreve código para o agente, só reindenta o que o brain_edit já alterou.
        if matches!(program, "rustfmt") {
            return None;
        }

        // wc sem arquivo e so um consumidor de pipe (cargo test | wc -l);
        // com arquivo, e leitura de codigo: brain_read ja devolve o tamanho.
        let has_file_operand = words
            .iter()
            .skip(1)
            .any(|word| !word.starts_with('-') && word != "-");

        let (replacement, label) = match program {
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "ugrep" => ("brain_search", program),
            "cat" | "bat" | "less" | "more" | "most" | "head" | "tail" | "nl" | "tac" | "strings"
            | "xxd" | "od" | "hexdump" => ("brain_read", program),
            "awk" | "gawk" | "mawk" | "cut" => ("brain_read", program),
            "sed" => {
                let edit = words.iter().skip(1).any(|w| w.starts_with("-i"));
                (if edit { "brain_edit" } else { "brain_read" }, program)
            }
            "wc" if has_file_operand => ("brain_read", program),
            "find" | "fd" | "fdfind" | "locate" | "tree" | "ls" | "dir" | "vdir" => {
                ("brain_map", program)
            }
            "tee" | "dd" | "truncate" => ("brain_write", program),
            "patch" => ("brain_patch", program),
            "git" => return git_target(words),
            "cargo" => return cargo_target(words),
            _ => {
                return Some(Blocked {
                    program: program.to_string(),
                    replacement: "",
                    detail: Some(OUTSIDE_GUARD),
                })
            }
        };
        Some(Blocked {
            program: label.to_string(),
            replacement,
            detail: None,
        })
    }

fn git_target(words: &[String]) -> Option<Blocked> {
        const GIT_DETAIL: &str = "Essa opção do git executa comandos. O brain_shell não é caminho para rodar código: use brain_exec para compilar e testar.";
        if words
            .iter()
            .skip(1)
            .any(|word| word == "-c" || word == "--exec" || word.starts_with("--exec="))
        {
            return Some(Blocked {
                program: "git -c/--exec".into(),
                replacement: "",
                detail: Some(GIT_DETAIL),
            });
        }
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
        let (replacement, label, detail) = match subcommand.as_str() {
            "grep" => ("brain_search", "git grep", None),
            "diff" => ("brain_diff", "git diff", None),
            // rev:path is file content, not a diff: brain_read { rev } serves it.
            "show" if words[index + 1..].iter().any(|w| is_revision_path(w)) => {
                ("brain_read", "git show <rev>:<path>", None)
            }
            "show" => ("brain_diff", "git show", None),
            "cat-file" if words[index + 1..].iter().any(|w| is_revision_path(w)) => {
                ("brain_read", "git cat-file <rev>:<path>", None)
            }
            "blame" => ("brain_why", "git blame", None),
            "apply" => ("brain_patch", "git apply", None),
            "mv" => ("brain_move", "git mv", None),
            "ls-files" => ("brain_map", "git ls-files", None),
            "ls-tree" => ("brain_map", "git ls-tree", None),
            // `-p` imprime o patch, que é o mesmo conteúdo de git diff/show.
            "log" if words[index + 1..].iter().any(|w| w == "-p" || w == "--patch") => {
                ("brain_diff", "git log -p", None)
            }
            "log" if words[index + 1..].iter().any(|w| w.starts_with("-L")) => {
                ("brain_why", "git log -L", None)
            }
            "stash" if words[index + 1..].iter().any(|w| w == "-p" || w == "--patch") => {
                ("brain_diff", "git stash show -p", None)
            }
            "format-patch" => ("brain_diff", "git format-patch", None),
            "difftool" | "mergetool" | "filter-branch" | "submodule" => {
                ("", "git (comando externo)", Some(GIT_DETAIL))
            }
            "bisect" if words[index + 1..].iter().any(|w| w == "run") => {
                ("", "git bisect run", Some(GIT_DETAIL))
            }
            _ => return None,
        };
        Some(Blocked {
            program: label.to_string(),
            replacement,
            detail,
        })
    }

/// `HEAD~1:src/lib.rs` names a file at a revision, which only
/// `brain_read { rev }` can serve. Object metadata (`git cat-file -t HEAD`) has
/// no file to read, so it stays uncovered.
fn is_revision_path(word: &str) -> bool {
    !word.starts_with('-')
        && matches!(
            word.split_once(':'),
            Some((rev, path)) if !rev.is_empty() && !path.is_empty()
        )
}

fn cargo_target(words: &[String]) -> Option<Blocked> {
        const CARGO_DETAIL: &str = "Esse subcomando do cargo executa código ou instala binário. Para compilar, testar e rodar lints, use brain_exec.";
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
        match subcommand.as_str() {
            "build" | "check" | "test" | "clippy" => Some(Blocked {
                program: format!("cargo {subcommand}"),
                replacement: "brain_exec",
                detail: None,
            }),
            "run" | "bench" | "install" => Some(Blocked {
                program: format!("cargo {subcommand}"),
                replacement: "",
                detail: Some(CARGO_DETAIL),
            }),
            _ => None,
        }
    }

const SUDO_VALUE_OPTIONS: &[&str] = &["-u", "-g", "-C", "-p", "-h", "-r", "-t", "-U", "-T"];
const ENV_VALUE_OPTIONS: &[&str] = &["-u", "-C", "-S", "--unset", "--chdir"];
const TIME_VALUE_OPTIONS: &[&str] = &["-f", "-o", "--format", "--output"];
const NICE_VALUE_OPTIONS: &[&str] = &["-n", "--adjustment"];
const STDBUF_VALUE_OPTIONS: &[&str] = &["-i", "-o", "-e", "--input", "--output", "--error"];
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

/// Interpreters run one-liners that read the tree exactly like `cat`:
/// `python3 -c 'open(...)'` is the same leak under another name.
fn is_interpreter(program: &str) -> bool {
    program.starts_with("python")
        || matches!(
            program,
            "pypy" | "pypy3" | "node" | "nodejs" | "bun" | "deno" | "perl" | "ruby" | "php"
                | "lua" | "luajit" | "Rscript" | "irb"
        )
}

/// Only inline code is inspected. `python3 tools/build.py` is an external tool,
/// not a read of the tree, so it stays uncovered.
fn interpreter_target(program: &str, words: &[String]) -> Option<Blocked> {
        const INLINE_FLAGS: &[&str] = &["-c", "-e", "-E", "-r", "-p", "--eval", "--print"];
        const INLINE_DETAIL: &str = "Interpretador não roda aqui, nem inline nem por script. Para ler, use brain_read ou brain_search; para alterar, brain_edit, brain_write ou brain_patch; para compilar e testar, brain_exec.";
        const SCRIPT_DETAIL: &str = "Script não é ferramenta externa: nada de interpretador. Para ler, use brain_read ou brain_search; para alterar, brain_edit, brain_write ou brain_patch; para compilar e testar, brain_exec.";
        let mut inline = None;
        for (index, word) in words.iter().enumerate().skip(1) {
            if INLINE_FLAGS.contains(&word.as_str()) {
                inline = words.get(index + 1);
                break;
            }
        }
        let (replacement, detail) = match inline {
            Some(script) if reads_files(script) => ("brain_read", INLINE_DETAIL),
            Some(script) if hunts_for_text(script) => ("brain_search", INLINE_DETAIL),
            Some(_) => ("brain_read", INLINE_DETAIL),
            None => ("brain_read", SCRIPT_DETAIL),
        };
        Some(Blocked {
            program: program.to_string(),
            replacement,
            detail: Some(detail),
        })
    }

/// The primitives that open, list or execute something. A one-liner that only
/// does arithmetic has none of them and is left alone.
fn reads_files(script: &str) -> bool {
    const READ_HINTS: &[&str] = &[
        "open(",
        "open (",
        "io.open",
        "read_text",
        "readlines",
        "readline(",
        "readFile",
        "readFileSync",
        "createReadStream",
        "File.read",
        "File.open",
        "IO.read",
        "require('fs')",
        "require(\"fs\")",
        "from 'fs'",
        "from \"fs\"",
        "fs.",
        "file_get_contents",
        "fopen(",
        "io.lines",
        "Dir[",
        "Dir.glob",
        "pathlib",
        "listdir",
        "scandir",
        "os.walk",
        "glob(",
        "glob.glob",
        "shutil",
        "subprocess",
        "system(",
        "popen",
        "execSync",
        "exec(",
        "shell_exec",
        "os.execute",
        "child_process",
        "spawn(",
        "__import__",
        "importlib",
        "codecs.open",
    ];
    READ_HINTS.iter().any(|hint| script.contains(hint))
}

/// `in`, `find`, `index` and friends mean the one-liner is hunting text,
/// which is `brain_search`; otherwise it is a read, which is `brain_read`.
fn hunts_for_text(script: &str) -> bool {
    const SEARCH_HINTS: &[&str] = &[
        "search", "find", "grep", "match", "index", " in ", "listdir", "walk", "glob", "scandir",
        "Dir[",
    ];
    SEARCH_HINTS.iter().any(|hint| script.contains(hint))
}

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
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    fn a_wc_with_a_file_operand_is_reading_code() {
        assert_eq!(
            blocked("wc -l crates/brain-kernel/src/*.rs"),
            Some(("wc".into(), "brain_read"))
        );
        assert_eq!(
            blocked("/usr/bin/wc -l src/lib.rs"),
            Some(("wc".into(), "brain_read"))
        );
    }

    #[test]
    fn a_wc_is_not_a_brain_tool_even_in_a_pipe() {
        assert_eq!(blocked("wc -l"), Some(("wc".into(), "")));
        assert_eq!(blocked("wc -l -"), Some(("wc".into(), "")));
        assert_eq!(
            blocked("git log --oneline | wc -l"),
            Some(("wc".into(), ""))
        );
        assert_eq!(
            blocked("wc -l src/lib.rs"),
            Some(("wc".into(), "brain_read"))
        );
    }

    #[test]
    fn unwraps_wrappers_and_nested_shells() {
        assert_eq!(
            blocked("sudo env FOO=1 bash -c 'rg token'"),
            Some(("rg".into(), "brain_search"))
        );
        assert_eq!(
            blocked("git branch | xargs grep foo"),
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
            blocked("git ls-files crates/brain-kernel/src"),
            Some(("git ls-files".into(), "brain_map"))
        );
        assert_eq!(
            blocked("git -C sub ls-tree HEAD src"),
            Some(("git ls-tree".into(), "brain_map"))
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
            blocked("git status > out.txt"),
            Some(("> out.txt".into(), "brain_write"))
        );
        assert_eq!(
            blocked("git status >> out.txt"),
            Some((">> out.txt".into(), "brain_write"))
        );
        assert_eq!(blocked("git status 2>&1 > /dev/null"), None);
    }

    #[test]
    fn a_nested_shell_is_blocked_before_its_writes() {
        // O shell em si já é negado, então o write externo não muda a decisão.
        assert_eq!(
            blocked("bash -c 'git status' > out.txt"),
            Some(("bash".into(), ""))
        );
        assert_eq!(
            blocked("bash -c 'echo hi' > out.txt"),
            Some(("echo".into(), ""))
        );
    }

    #[test]
    fn only_git_and_formatters_run() {
        assert_eq!(blocked("git status"), None);
        assert_eq!(blocked("git commit -m 'x'"), None);
        assert_eq!(blocked("git restore src/lib.rs"), None);
        assert_eq!(blocked("git log --oneline"), None);
        assert_eq!(blocked("cargo fmt"), None);
        assert_eq!(blocked("cargo fmt -- --check"), None);
        assert_eq!(blocked("rustfmt crates/brain-exec/src/guard.rs"), None);
        // Fora do git e do formatador, o brain_shell não roda nada.
        assert_eq!(blocked("mkdir -p tmp"), Some(("mkdir".into(), "")));
        assert_eq!(blocked("make build"), Some(("make".into(), "")));
        assert_eq!(blocked("echo hello"), Some(("echo".into(), "")));
        assert_eq!(
            blocked("curl https://example.com"),
            Some(("curl".into(), ""))
        );
        assert_eq!(
            blocked("cargo run -- --root ."),
            Some(("cargo run".into(), ""))
        );
        // git também não é atalho para executar comandos nem para imprimir patch.
        assert_eq!(
            blocked("git -c core.pager='sh -c cat' log"),
            Some(("git -c/--exec".into(), ""))
        );
        assert_eq!(
            blocked("git rebase --exec make main"),
            Some(("git -c/--exec".into(), ""))
        );
        assert_eq!(
            blocked("git difftool"),
            Some(("git (comando externo)".into(), ""))
        );
        assert_eq!(
            blocked("git bisect run make"),
            Some(("git bisect run".into(), ""))
        );
        assert_eq!(
            blocked("git log -p"),
            Some(("git log -p".into(), "brain_diff"))
        );
        assert_eq!(
            blocked("git stash show -p"),
            Some(("git stash show -p".into(), "brain_diff"))
        );
    }

    #[test]
    fn the_message_names_the_program_and_the_replacement() {
        let text = message(&check("grep x").unwrap());
        assert!(text.contains("`grep`"));
        assert!(text.contains("brain_search"));
        assert!(text.contains("brain_shell"));
    }
    #[test]
    fn a_revision_path_is_a_read_not_a_diff() {
        assert_eq!(
            blocked("git cat-file -p HEAD:crates/a.rs"),
            Some(("git cat-file <rev>:<path>".into(), "brain_read"))
        );
        assert_eq!(
            blocked("git cat-file blob v0.3.0:src/a.rs"),
            Some(("git cat-file <rev>:<path>".into(), "brain_read"))
        );
        assert_eq!(
            blocked("git show HEAD~1:src/a.rs"),
            Some(("git show <rev>:<path>".into(), "brain_read"))
        );
        assert_eq!(
            blocked("git show HEAD"),
            Some(("git show".into(), "brain_diff"))
        );
        // No file behind the object: nothing to replace.
        assert_eq!(blocked("git cat-file -t HEAD"), None);
    }

    #[test]
    fn scripts_are_not_external_tools() {
        assert_eq!(
            blocked("python3 -c \"open('src/a.rs').read()\""),
            Some(("python3".into(), "brain_read"))
        );
        assert_eq!(
            blocked("node -e \"require('fs').readFileSync('src/a.rs')\""),
            Some(("node".into(), "brain_read"))
        );
        // O caso que escapava: script em arquivo não é ferramenta externa.
        assert_eq!(
            blocked("python3 tools/build.py"),
            Some(("python3".into(), "brain_read"))
        );
        assert_eq!(
            blocked("python3 .agents/skills/domain-framework/scripts/domain_audit.py --root ."),
            Some(("python3".into(), "brain_read"))
        );
        assert_eq!(
            blocked("ruby script.rb"),
            Some(("ruby".into(), "brain_read"))
        );
        // Execução direta pelo shebang também não passa.
        assert_eq!(
            blocked("./tools/build.py --root ."),
            Some(("build.py".into(), ""))
        );
        assert_eq!(blocked("bash tools/build.sh"), Some(("bash".into(), "")));
        assert_eq!(blocked("sh -c 'git status'"), Some(("sh".into(), "")));
        assert_eq!(blocked("eval 'git status'"), Some(("eval".into(), "")));
    }

    #[test]
    fn a_heredoc_into_an_interpreter_is_inline_code() {
        assert_eq!(
            blocked("python3 - <<'PY'\nfrom pathlib import Path\nPath('a.rs').write_text('x')\nPY"),
            Some(("python3 <<".into(), "brain_edit"))
        );
        assert_eq!(
            blocked("bash <<'EOF'\ngit status\nEOF"),
            Some(("bash <<".into(), "brain_edit"))
        );
        // Data heredocs stay uncovered: git reads the message, not code.
        assert_eq!(blocked("git commit -F - <<'EOF'\nmsg\nEOF"), None);
    }

    #[test]
    fn a_heredoc_is_one_operator_not_two_inputs() {
        assert_eq!(
            tokenize("python3 - <<'PY'"),
            vec![
                Token::Word("python3".into()),
                Token::Word("-".into()),
                Token::Op(Op::Heredoc),
                Token::Word("PY".into()),
            ]
        );
        assert!(segments("python3 - <<'PY'")[0].heredoc);
        assert!(!segments("python3 < script.py")[0].heredoc);
    }
}
