# Guarda de comandos no brain_shell — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Recusar, antes do `spawn`, todo comando de `brain_shell` cujo papel já é coberto por uma ferramenta brain, devolvendo um erro que nomeia a ferramenta certa.

**Architecture:** Um módulo puro `guard` tokeniza o comando, separa segmentos de pipeline, desembrulha wrappers e classifica cada programa contra uma denylist derivada de `docs/opencode.md`. O `ShellTool` chama a guarda antes de montar o `Command` e registra a recusa no log de uso. Uma flag `--shell-guard` (default ligada) permite desligar.

**Tech Stack:** Rust, tokio, serde, clap. Sem dependência nova.

**Spec:** `docs/superpowers/specs/2026-10-29-guard-shell-design.md`

## Global Constraints

- Nenhuma dependência nova; tokenização é escrita à mão.
- `--shell-guard` / `BRAIN_SHELL_GUARD`, default `true`, `clap::ArgAction::Set`.
- A guarda roda **antes** do `spawn`: comando barrado não executa nada.
- Mensagens em português, no tom das demais tools.
- Recusa = `CallToolResult::error` + entrada de uso com `status: "blocked"`.
- Não alterar contrato de nenhuma outra tool nem a ordem de `tools/list`.
- `cargo fmt` e `cargo clippy` limpos ao fim de cada task.

## Review Focus

- Opções antes do subcomando não podem evadir: `git -C sub diff`, `cargo +nightly test`.
- Wrappers: `bash -c 'grep …'`, `sudo env FOO=1`, `xargs grep`, `eval`.
- Redirecionamento: `2>&1` não é escrita; `> /dev/null` não é escrita; `> arquivo` é.
- Formas lexicais: `"grep"`, `/usr/bin/grep`, `./rg` continuam sendo o programa.
- Falsos positivos quebram a porta de escape: `git status`, `cargo fmt`, `mkdir`, `make` devem passar.

---

### Task 1: Tokenizador e segmentos de pipeline

**Files:**
- Create: `crates/brain-exec/src/guard.rs`
- Modify: `crates/brain-exec/src/lib.rs` (adicionar `pub mod guard;`)

**Interfaces:**
- Consumes: nada.
- Produces:
  - `pub enum Token { Word(String), Op(Op) }`
  - `pub enum Op { Pipe, And, Or, Semi, Background, Redirect, Append, RedirectAll, Input }`
  - `pub struct Segment { pub words: Vec<String>, pub writes: Vec<Write> }`
  - `pub struct Write { pub append: bool, pub target: String }`
  - `pub fn tokenize(command: &str) -> Vec<Token>`
  - `pub fn segments(command: &str) -> Vec<Segment>`

- [ ] **Step 1: Write the failing tests**

Em `crates/brain-exec/src/guard.rs`:

```rust
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
            tokenize(r#"echo a\ b"#),
            vec![Token::Word("echo".into()), Token::Word("a b".into())]
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p brain-exec guard::`
Expected: FAIL — `tokenize`/`segments`/`Token` not found.

- [ ] **Step 3: Implement the lexer and segmenter**

Regras: espaços separam palavras; `'` literal até a próxima aspa; `"` aceita `\"` e `\\`; `\` fora de quotes escapa o próximo caractere; operadores reconhecidos do mais longo para o mais curto (`||`, `&&`, `>>`, `&>`, `|&` como `Pipe`, `>`, `<`, `|`, `;`, `&`). Se o buffer atual for só dígitos e vier `>`, limpe o buffer (fd) e emita `Redirect`. `>` seguido de `&` e dígitos é duplicação de fd: emita `Redirect` e deixe o alvo como `Word("&1")`; `segments` ignora alvos `&N`. `#` no início de uma palavra (fora de quotes) inicia comentário até o fim da linha; `segments` inicia novo `Segment` em `Pipe/And/Or/Semi/Background`; `Redirect/Append/RedirectAll` consomem a próxima `Word` como `Write { append, target }`; `Input` descarta o próximo alvo.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p brain-exec guard::`
Expected: PASS (5 testes).

- [ ] **Step 5: Commit**

```bash
git add crates/brain-exec/src/guard.rs crates/brain-exec/src/lib.rs
git commit -m "feat(exec): tokenize shell segments for the brain_shell guard"
```

---

### Task 2: Classificação e mensagem de recusa

**Files:**
- Modify: `crates/brain-exec/src/guard.rs`

**Interfaces:**
- Consumes: `tokenize`, `segments`, `Segment` da Task 1.
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Blocked { pub program: String, pub replacement: &'static str }`
  - `pub fn check(command: &str) -> Option<Blocked>`
  - `pub fn message(blocked: &Blocked) -> String`

- [ ] **Step 1: Write the failing tests**

Acrescente ao `mod tests`:

```rust
fn blocked(command: &str) -> Option<(&'static str, &'static str)> {
    check(command).map(|b| (b.program.as_str(), b.replacement))
}

#[test]
fn blocks_search_programs_anywhere_in_the_pipeline() {
    assert_eq!(blocked("git log | grep fix"), Some(("grep", "brain_search")));
    assert_eq!(blocked("rg TODO src"), Some(("rg", "brain_search")));
    assert_eq!(blocked("sed -n '1,20p' src/lib.rs"), Some(("sed", "brain_read")));
    assert_eq!(blocked("sed -i 's/a/b/' src/lib.rs"), Some(("sed", "brain_edit")));
    assert_eq!(blocked("find . -name '*.rs'"), Some(("find", "brain_map")));
}

#[test]
fn unwraps_wrappers_and_nested_shells() {
    assert_eq!(
        blocked("sudo env FOO=1 bash -c 'rg token'"),
        Some(("rg", "brain_search"))
    );
    assert_eq!(blocked("printf x | xargs grep foo"), Some(("grep", "brain_search")));
    assert_eq!(blocked("eval "cat Cargo.toml""), Some(("cat", "brain_read")));
    assert_eq!(blocked("/usr/bin/grep needle Cargo.toml"), Some(("grep", "brain_search")));
    assert_eq!(blocked("\"grep\" needle Cargo.toml"), Some(("grep", "brain_search")));
}

#[test]
fn reads_git_and_cargo_subcommands_past_global_options() {
    assert_eq!(blocked("git -C sub diff --stat"), Some(("git diff", "brain_diff")));
    assert_eq!(blocked("git log -L 1,20:src/a.rs"), Some(("git log -L", "brain_why")));
    assert_eq!(blocked("cargo +nightly test --all"), Some(("cargo test", "brain_exec")));
    assert_eq!(blocked("cargo clippy -- -D warnings"), Some(("cargo clippy", "brain_exec")));
}

#[test]
fn blocks_file_writes_but_not_fd_duplication_or_dev_null() {
    assert_eq!(blocked("echo hi > out.txt"), Some(("> out.txt", "brain_write")));
    assert_eq!(blocked("echo hi >> out.txt"), Some((">> out.txt", "brain_write")));
    assert_eq!(blocked("cargo test 2>&1 > /dev/null"), None);
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
    assert!(text.contains("\`grep\`"));
    assert!(text.contains("brain_search"));
    assert!(text.contains("brain_shell"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p brain-exec guard::`
Expected: FAIL — `check`/`Blocked`/`message` not found.

- [ ] **Step 3: Implement classification**

Ordem por segmento:
1. Remover atribuições `VAR=...` iniciais e prefixos `sudo`, `env`, `time`, `command`, `nohup`, `nice`, `stdbuf` (com seus argumentos `-n N`, `-i`, etc.).
2. Programa = `Path::new(word).file_name()`, exceto `busybox`, que cede a vez à palavra seguinte.
3. Recursão: `bash|sh|zsh|dash|ksh` com flag contendo `c` → `check` no argumento seguinte; `eval` → `check` no restante unido por espaço; `xargs` → pular opções e classificar o programa seguinte (sem programa = `echo`, permitido).
4. Tabela: `grep/egrep/fgrep/rg/ag/ack/ugrep` → `brain_search`; `cat/bat/less/more/most/head/tail/nl/tac/strings/xxd/od/hexdump` → `brain_read`; `awk/gawk/mawk/cut` → `brain_read`; `sed` com argumento começando em `-i` → `brain_edit`, senão `brain_read`; `find/fd/fdfind/locate/tree/ls/dir/vdir` → `brain_map`; `tee/dd/truncate` → `brain_write`; `patch` → `brain_patch`.
5. `git`: pular `-C <path>`, `-c k=v`, `--git-dir=…`, `--no-pager`; subcomando `grep` → `brain_search`, `diff`/`show` → `brain_diff`, `blame` → `brain_why`, `log` com argumento `-L…` → `brain_why`, `apply` → `brain_patch`.
6. `cargo`: pular `+toolchain`; subcomando em `build|check|test|clippy` → `brain_exec`.
7. Se nada barrou, para cada `Write` da Task 1 cujo `target` não seja `/dev/null`, `/dev/stdout`, `/dev/stderr` nem `&N`: `Blocked { program: format!("{} {}", if write.append { ">>" } else { ">" }, write.target), replacement: "brain_write" }`.

`message` devolve:

```text
### brain_shell — comando bloqueado

`{program}` é substituído por `{replacement}`.

Use `{replacement}` em vez do shell. Pesquisa e leitura de código são
`brain_search`, `brain_read`, `brain_map`, `brain_context_graph` e
`brain_impact_check`. `brain_shell` é só para git, formatador e ferramenta externa.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p brain-exec guard::`
Expected: PASS (11 testes).

- [ ] **Step 5: Commit**

```bash
git add crates/brain-exec/src/guard.rs
git commit -m "feat(exec): block shell commands already covered by brain tools"
```

---

### Task 3: Integração no ShellTool e flag de configuração

**Files:**
- Modify: `crates/brain-exec/src/shell_tool.rs`
- Modify: `crates/brain-kernel/src/config.rs` (junto de `shell_disabled`)
- Modify: `crates/brain-tools/src/main.rs:136-150`
- Test: `crates/brain-tools/tests/config_test.rs`

**Interfaces:**
- Consumes: `guard::check`, `guard::message` da Task 2.
- Produces: `ShellTool::with_guard(self, bool) -> Self`; `Config.shell_guard: bool`.

- [ ] **Step 1: Write the failing tests**

Em `shell_tool.rs`:

```rust
#[tokio::test]
async fn a_covered_command_is_refused_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(ShellUsageLog::new(dir.path().join("usage.jsonl"), false));
    let tool = ShellTool::new(
        dir.path(),
        Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
        2000,
        Duration::from_secs(5),
    )
    .with_guard(true)
    .with_usage_log(Some(log.clone()));

    let out = tool
        .run(ShellParams { command: "grep needle Cargo.toml".into(), timeout_secs: None, budget_tokens: None })
        .await;
    assert_eq!(out.is_error, Some(true));
    assert!(out.content[0].text.contains("brain_search"));

    let entry: ShellUsageEntry =
        serde_json::from_str(std::fs::read_to_string(log.path()).unwrap().trim()).unwrap();
    assert_eq!(entry.status, "blocked");
    assert!(entry.error.unwrap().contains("brain_search"));
}

#[tokio::test]
async fn the_guard_can_be_turned_off() {
    let out = ShellTool::new(
        ".",
        Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
        2000,
        Duration::from_secs(5),
    )
    .with_guard(false)
    .run(ShellParams { command: "grep brain-exec Cargo.toml".into(), timeout_secs: None, budget_tokens: None })
    .await;
    assert!(out.is_error.is_none());
    assert!(out.content[0].text.contains("brain-exec"));
}
```

Em `config_test.rs`:

```rust
#[test]
fn the_shell_guard_is_on_by_default() {
    assert!(Config::parse_from(["brain-tools"]).shell_guard);
}

#[test]
fn the_shell_guard_can_be_turned_off() {
    assert!(!Config::parse_from(["brain-tools", "--shell-guard", "false"]).shell_guard);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p brain-exec shell_tool::` e `cargo test -p brain-tools --test config_test`
Expected: FAIL — `with_guard`/`shell_guard` not found.

- [ ] **Step 3: Implement the wiring**

- `ShellTool`: campo `guard: bool` (default `true` em `new`), `with_guard`.
- Em `run`, depois do cálculo de `timeout` e antes do `Command::new`: se `self.guard`, `guard::check(&params.command)`; se houver, `write_usage` com `status: "blocked"` e `error: Some(guard::message(&b))`, e `return CallToolResult::error(...)`.
- `Config`: `--shell-guard`, env `BRAIN_SHELL_GUARD`, `default_value_t = true`, `ArgAction::Set`.
- `main.rs`: `.with_guard(config.shell_guard)` na construção do `ShellTool`.
- Descrição do `schema()`: acrescentar “Comandos cobertos por brain_search/brain_read/brain_map/brain_edit/brain_exec são recusados.”

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p brain-exec` e `cargo test -p brain-tools --test config_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/brain-exec/src/shell_tool.rs crates/brain-kernel/src/config.rs crates/brain-tools/src/main.rs crates/brain-tools/tests/config_test.rs
git commit -m "feat(exec): enforce the shell guard in brain_shell"
```

---

### Task 4: AGENTS.md e documentação

**Files:**
- Create: `AGENTS.md`
- Modify: `docs/brain-rules.md:14`
- Modify: `docs/opencode.md` (tabela de substituições e seção “O que quebra”)

**Interfaces:**
- Consumes: nomes de flag e comportamento das Tasks 1–3.
- Produces: instrução carregada automaticamente pelo OpenCode.

- [ ] **Step 1: Write AGENTS.md**

Conteúdo mínimo: o brain-tools é a fonte de verdade; ordem de prioridade (`brain_search`, `brain_context_graph`/`brain_impact_check`, `brain_read`, `brain_map`, `brain_edit`, `brain_write`/`brain_patch`, `brain_exec`); tabela “não use → use” (`grep/rg/find/sed/cat/head/tail/ls` → `brain_search`/`brain_read`/`brain_map`; `cargo build|check|test|clippy` → `brain_exec`; `git diff|show` → `brain_diff`; `git log -L|blame` → `brain_why`; `sed -i`/`tee`/`patch` → `brain_edit`/`brain_write`/`brain_patch`); aviso de que o `brain_shell` recusa esses comandos por padrão e que contornar via script ou interpretador viola a regra; e a exigência de dizer explicitamente quando uma chamada MCP falhar.

- [ ] **Step 2: Align the docs**

- `docs/brain-rules.md` item 7: acrescentar “a guarda do `brain_shell` recusa comandos cobertos por outra tool”.
- `docs/opencode.md`: na linha do `brain_shell`, citar a guarda; na seção “O que quebra”, registrar que `grep`/`sed`/`find`/`cat` via shell passam a ser recusados e o caminho é a tool brain correspondente.

- [ ] **Step 3: Verify the text landed**

Run: `test -f AGENTS.md && test $(wc -l < AGENTS.md) -ge 10`; depois `brain_read AGENTS.md` e `brain_read docs/brain-rules.md`
Expected: arquivo existe, contagens ≥ 1.

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md docs/brain-rules.md docs/opencode.md
git commit -m "docs: state the brain_shell guard and the shell-to-brain map"
```

---

## Deployment note

O `opencode.json` aponta para `target/release/brain-tools`. Só depois de `cargo build --release` e do reinício do daemon a guarda vale para as sessões do agente. Enquanto ela estiver ativa, `cargo test`/`cargo build` via `brain_shell` são recusados de propósito: use `brain_exec`.
