## brain-tools é a fonte de verdade

O MCP `brain-tools` é a forma de olhar o código deste projeto. O resultado dele
**é** a leitura do arquivo: trecho verbatim, com `path:start-end`.

Ordem de prioridade:

1. `brain_search` — onde está e como funciona algo.
2. `brain_context_graph` / `brain_impact_check` — quem chama, o que quebra.
3. `brain_read` — esqueleto do arquivo ou corpo de um símbolo.
4. `brain_map` — orientação quando você não sabe onde está.
5. `brain_edit` — editar o corpo de um símbolo já visto, com o hash do `brain_read`.
6. `brain_write` / `brain_patch` — arquivo inteiro ou diff unificado.
7. `brain_exec` — build, check, test, clippy. `brain_shell` só para git/formatador/ferramenta externa; a guarda recusa comandos cobertos por outra tool (`--shell-guard false` desliga).

Não use `read`, `grep`, `glob` nem `shell` para conferir o que o brain-tools já
entregou. Score baixo ordena candidatas entre si; não mede veracidade. Se a busca
não respondeu, refine a busca — não abra o arquivo.

Para editar um símbolo existente, use `brain_edit` com o `expected_hash` do
`brain_read` e o `new_body` completo. `Read` + `Edit` só é legítimo para um símbolo
que você ainda não viu; depois disso, o hash é a garantia de versão. Arquivo inteiro
ou diff: `brain_write` / `brain_patch`; terminal: `brain_shell`, não `shell`.

Se uma chamada MCP falhar, diga isso explicitamente. Não caia em `grep` em
silêncio: a degradação precisa ser visível.
