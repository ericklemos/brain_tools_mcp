# brain-tools — regras do projeto

O MCP `brain-tools` é a forma de olhar este código. O resultado dele **é** a
leitura do arquivo: trecho verbatim, com `path:start-end`.

## Objetivo

Economia de tokens, sim — mas o ponto principal é a **seleção inteligente de
conteúdo**. O `brain-tools` repassa ao agente o que é de fato relevante para a
tarefa: o corpo do símbolo, o chamador, o impacto, o erro. O resto fica fora do
contexto.

Um contexto melhor filtrado, mais bem construído e mais centrado reduz distração
e erro de atenção. Prefira a resposta cirúrgica a uma leitura ampla: cada trecho
a mais compete com o que importa.

## Ordem de prioridade

1. `brain_search` — onde está e como funciona algo.
2. `brain_context_graph` / `brain_impact_check` — quem chama, o que quebra.
3. `brain_read` — esqueleto do arquivo ou corpo de um símbolo.
4. `brain_map` — orientação quando você não sabe onde está.
5. `brain_edit` — editar o corpo de um símbolo já visto, com o hash do `brain_read`.
6. `brain_write` / `brain_patch` — arquivo inteiro ou diff unificado.
7. `brain_exec` — build, check, test, clippy.
8. `brain_shell` — só git e formatador; qualquer outro comando é negado.

## Não use → use

| Não use | Use |
|---|---|
| `grep`, `rg`, `ag`, `ack` | `brain_search` |
| `cat`, `head`, `tail`, `sed`, `awk`, `cut`, `less` | `brain_read` |
| `find`, `ls`, `tree`, `fd` | `brain_map` |
| `sed -i`, `tee`, redirecionamento para arquivo | `brain_edit` / `brain_write` |
| `patch`, `git apply` | `brain_patch` |
| `mv`, `cp`, `git mv` | `brain_move` (`copy: true` para copiar) |
| `cargo build`, `cargo check`, `cargo test`, `cargo clippy` | `brain_exec` |
| `python3 -c`, `node -e`, `perl -e`, `php -r` | `brain_read` / `brain_search` |
| `python3 script.py`, `./script.py`, `bash script.sh` | `brain_read` / `brain_exec` |
| `python3 - <<EOF`, `bash <<EOF` | `brain_edit` |
| `git diff`, `git show` | `brain_diff` |
| `git cat-file <rev>:<path>`, `git show <rev>:<path>` | `brain_read` com `rev` |
| `git log -L`, `git blame` | `brain_why` |

O `brain_shell` só roda git e formatador — a guarda vem ligada
(`--shell-guard false` desliga, `BRAIN_SHELL_GUARD` também controla). Interpretador,
script, shell e comando fora da allowlist são negados; contornar por alias viola a
regra mesmo com a guarda desligada.

Se uma chamada MCP falhar, diga isso explicitamente. Não caia em `grep` em
silêncio: a degradação precisa ser visível.

## Como construir as ferramentas

O agente vai tentar burlar: pedir o arquivo inteiro, reler o mesmo trecho,
usar um comando que devolve tudo. A ferramenta precisa ser **chata** com esse
comportamento, não complacente. O padrão é a resposta cirúrgica; ampliar o
contexto é exceção explícita.

Regras de desenho:

- **Nunca devolva o arquivo inteiro por padrão.** A resposta é o símbolo, o
  esqueleto, o chamador, o erro agrupado. Se não couber, trunque e ofereça
  `brain_peek`; não aumente o orçamento para caber tudo.
- **Não abra atalho para contornar.** Uma ferramenta nova só entra se for
  mais seletiva que a alternativa que substitui (`grep`, `cat`, `git diff`).
  Se a única implementação possível é despejar tudo, não crie a ferramenta.
- **Recuse em vez de degradar.** Comando coberto por outra ferramenta é negado
  com a mensagem que indica a ferramenta certa. Erro explícito, nunca saída
  vazia nem fallback silencioso.
- **Negue por padrão.** Guarda, permissões e allowlist vêm ligadas. O agente
  precisa de um motivo nomeado para ver mais, e o motivo vive no parâmetro
  (`symbol`, `path`, `detail`), não num "modo completo".
- **Cada parâmetro amplia o foco, não o despejo.** `detail`, `depth`,
  `max_results` e `budget_tokens` existem para cortar melhor, não para
  liberar leitura ampla.
- **Filtre antes de enviar.** Ranqueie, deduplique, agrupe e corte o que não
  responde à pergunta. Contexto é orçamento de atenção.
- **Robustez acima de conveniência.** Valide hash, limite ao root, recuse
  escrita obsoleta e devolva verbatim com `path:start-end`. A resposta é a
  leitura, não um resumo a conferir.
- **Prove que a barreira funciona.** Todo atalho fechado precisa de teste: a
  chamada crua falha, a cirúrgica basta. Regra sem teste é só intenção.
