//! Guard for `brain_shell`: refuses commands a brain tool already covers.
//!
//! The guard is behavioural, not a sandbox: it inspects the command string
//! before `bash` sees it. A command that a brain tool replaces is refused with
//! the name of that tool, so the fallback to `grep`/`sed`/`cat` stops being
//! silent.

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
}
