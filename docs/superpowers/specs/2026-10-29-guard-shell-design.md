# Guarda de comandos no brain_shell

Data: 2026-10-29
Status: aguardando aprovação

## Intenção

O `brain_shell` é a porta de escape para o que o `brain_exec` não cobre (git,
formatador, ferramenta externa). Na prática ele virou atalho para `grep`, `sed` e
`cat`, que é exatamente o comportamento que o projeto existe para eliminar. A
guarda recusa o comando antes do `spawn` e devolve um erro que ensina a ferramenta
certa — a degradação deixa de ser silenciosa.

## Goals

- Bloquear, por padrão, todo comando cujo papel já é coberto por uma ferramenta
  brain, em qualquer posição do pipeline (`|`, `&&`, `||`, `;`, `&`, newline).
- Desembrulhar wrappers (`sudo`, `env`, `time`, `command`, `nohup`, `nice`,
  `bash -c`, `sh -c`, `eval`, `xargs`, `busybox`) antes de classificar.
- Devolver `CallToolResult::error` nomeando o programa barrado e a ferramenta
  brain que o substitui, com o motivo em uma linha.
- Registrar a recusa no `.brain/shell-usage.jsonl` com status `blocked`.
- Deixar a regra escrita no `AGENTS.md` da raiz (hoje inexistente), que é o
  arquivo que o OpenCode carrega sozinho.

## Non-goals

- Sandbox ou fronteira de segurança. `make`, um script `.sh` do próprio repo ou
  `python -c` continuam podendo ler arquivos; a guarda é de comportamento, como o
  resto do projeto já assume.
- Bloquear programas que o brain não cobre: `git status/add/commit/push/log`,
  `cargo fmt`, `rustfmt`, `mkdir`, `rm`, `mv`, `cp`, `curl`, `make`, `npm`, `docker`.
- Detectar alias de shell, funções exportadas ou `$()` avaliado em runtime.

## Anti-goals

- Quebrar `git commit`, `git status`, formatação ou o fluxo de `brain_patch`.
- Bloquear um pipeline legítimo só porque ele contém um filtro de saída. A lista
  é por programa, não por substring: `git log --oneline` passa; `git log | grep`
  não.

## Approaches

1. Regex na string do comando. Descartada: fura com `bash -c` e `xargs` e dá
   falso-positivo em pipeline.
2. **Denylist por segmento, com desembrulho de wrappers.** Escolhida pelo parceiro.
   Tokeniza o pipeline, classifica cada segmento e barra o programa coberto.
3. Allowlist de programas. Descartada como padrão: quebra npm/docker/make, que são
   o motivo de o `brain_shell` existir. Poderia virar flag depois.

## Onde vive

- `crates/brain-exec/src/guard.rs` (novo): `pub fn check(command: &str) -> Option<Blocked>`,
  puro, sem I/O. `Blocked { program, replacement, reason }`.
- `ShellTool` ganha `guard: bool` (`with_guard`); `run()` chama `guard::check`
  antes de montar o `Command`. Recusa não escreve nada e não spawna.
- `Config`: `--shell-guard` (env `BRAIN_SHELL_GUARD`), default `true`,
  `ArgAction::Set`, no mesmo estilo de `--shell-usage-log`.
- `main.rs` passa `config.shell_guard` para o `ShellTool`.

## Classificação

Tokenização própria (sem dependência nova): quebra em operadores fora de quotes,
remove redirecionamentos do segmento, pula atribuições `VAR=...` e prefixos
conhecidos, e recursa em `sh -c`/`bash -c`/`eval`/`xargs`.

| Programa (ou subcomando) | Ferramenta |
|---|---|
| `grep`, `egrep`, `fgrep`, `rg`, `ag`, `ack`, `ugrep`, `git grep` | `brain_search` |
| `cat`, `bat`, `less`, `more`, `most`, `head`, `tail`, `nl`, `tac`, `strings`, `xxd`, `od`, `hexdump` | `brain_read` |
| `sed` (sem `-i`), `awk`, `gawk`, `mawk`, `cut` | `brain_search` / `brain_read` |
| `sed -i`, `perl -i`, `awk -i inplace` | `brain_edit` |
| `find`, `fd`, `fdfind`, `locate`, `tree`, `ls`, `dir`, `vdir` | `brain_map` |
| `tee`, `dd`, `truncate`, `patch`, `git apply` | `brain_write` / `brain_patch` |
| `cargo build`, `cargo check`, `cargo test`, `cargo clippy` | `brain_exec` |
| `git diff`, `git show` | `brain_diff` |
| `git log -L`, `git blame` | `brain_why` |

Redirecionamento de escrita (`>`, `>>`, `&>`) para caminho que não seja
`/dev/null` nem descritor (`&1`, `&2`) é bloqueado como `brain_write`.

## Contrato de retorno

```
### brain_shell — comando bloqueado

`grep` é substituído por `brain_search`.
Use `brain_search` para localizar símbolos e trechos; `brain_context_graph` para
quem chama; `brain_read` para o corpo de um símbolo. `brain_shell` é só para git,
formatador e ferramenta externa.
```

Status HTTP não existe aqui: é `CallToolResult::error`, com `is_error: true` e uma
entrada de uso com `status: "blocked"` e `error` preenchido.

## AGENTS.md

Criar na raiz, curto, com a ordem de prioridade (a mesma de `docs/brain-rules.md`)
e a tabela "não use → use". Registrar que a guarda do `brain_shell` recusa os
comandos cobertos e que contornar por script/interpretador é violação da regra,
não do código.

## Docs

Ajustar `docs/brain-rules.md` (item 7) e `docs/opencode.md` (tabela e seção de
custos) para citar a guarda e a flag. Sem mudança de contrato das outras tools.

## Riscos

- `ls` e `cat` são usados em fluxos legítimos de inspeção de arquivos não-código;
  eles serão barrados e o agente terá que usar `brain_read`. É o custo pedido.
- O binário servido pelo `opencode.json` é `target/release/brain-tools`: só vale
  depois de `cargo build --release` e reinício do daemon.
- Uma guarda com bug pode bloquear `git status`; os testes cobrem os permitidos
  tanto quanto os barrados.

## O que fica para o builder

Testes unitários da classificação (cada categoria, pipeline, quotes, `bash -c`,
`xargs`, redirect, permitidos), teste de integração do `ShellTool` (não spawna,
erro nomeia a ferramenta, log `blocked`) e teste de config da flag.
