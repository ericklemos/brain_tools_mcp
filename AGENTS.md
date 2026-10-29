# brain-tools — regras do projeto

O MCP `brain-tools` é a forma de olhar este código. O resultado dele **é** a
leitura do arquivo: trecho verbatim, com `path:start-end`.

## Ordem de prioridade

1. `brain_search` — onde está e como funciona algo.
2. `brain_context_graph` / `brain_impact_check` — quem chama, o que quebra.
3. `brain_read` — esqueleto do arquivo ou corpo de um símbolo.
4. `brain_map` — orientação quando você não sabe onde está.
5. `brain_edit` — editar o corpo de um símbolo já visto, com o hash do `brain_read`.
6. `brain_write` / `brain_patch` — arquivo inteiro ou diff unificado.
7. `brain_exec` — build, check, test, clippy.
8. `brain_shell` — só git, formatador e ferramenta externa.

## Não use → use

| Não use | Use |
|---|---|
| `grep`, `rg`, `ag`, `ack` | `brain_search` |
| `cat`, `head`, `tail`, `sed`, `awk`, `cut`, `less` | `brain_read` |
| `find`, `ls`, `tree`, `fd` | `brain_map` |
| `sed -i`, `tee`, redirecionamento para arquivo | `brain_edit` / `brain_write` |
| `patch`, `git apply` | `brain_patch` |
| `cargo build`, `cargo check`, `cargo test`, `cargo clippy` | `brain_exec` |
| `git diff`, `git show` | `brain_diff` |
| `git log -L`, `git blame` | `brain_why` |

O `brain_shell` recusa esses comandos por padrão — a guarda vem ligada
(`--shell-guard false` desliga, `BRAIN_SHELL_GUARD` também controla). Contornar
por script próprio, interpretador ou alias viola a regra, mesmo quando o código
aceitar: a guarda é de comportamento, não um sandbox.

Se uma chamada MCP falhar, diga isso explicitamente. Não caia em `grep` em
silêncio: a degradação precisa ser visível.
