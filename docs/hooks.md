# Integração total com o Claude Code

Duas camadas, com poderes diferentes:

| Camada | Onde vive | Garante? |
|---|---|---|
| Instrução | `~/.claude/CLAUDE.md` | Não — orienta o agente |
| Hook | `~/.claude/settings.json` | Sim — o harness recusa a chamada |

A instrução convence. O hook **sobrepõe**. Para o comportamento "não releia o que o
brain-tools já entregou" é preciso o hook, porque a decisão de reler é do modelo.

## 1. Instrução — bloco para `~/.claude/CLAUDE.md`

```markdown
## brain-tools é a fonte de verdade

O MCP `brain-tools` é a primeira e principal forma de olhar o código deste projeto.
Não é uma busca auxiliar: o resultado dele **é** a leitura do arquivo.

### Ordem de prioridade

1. `brain_search` — onde está e como funciona algo.
2. `brain_context_graph` / `brain_impact_check` — quem chama, o que consome, o que quebra.
3. Só depois: `Read`, `Grep` ou `Glob`.

### Não verifique o que a ferramenta já entregou

- O trecho devolvido é o **corpo verbatim do símbolo**, com `path:start-end`. Trate-o
  como conteúdo já lido. **Não** chame `Read` no mesmo arquivo para conferir.
- Score baixo não é motivo para desconfiar: ele ordena candidatas entre si; não mede
  veracidade. O resultado traz a contagem de candidatas para dizer se algo ficou de fora.
- `Read` é legítimo apenas para: (a) imports/contexto fora da faixa devolvida,
  (b) um símbolo que você **ainda não viu** — depois da primeira leitura, `brain_edit`
  substitui `Read` + `Edit`, (c) quando a resposta declara corte. Nesses casos leia
  **só a faixa necessária** (`offset`/`limit`).
- **Editar símbolo existente usa `brain_edit`** com o `path`, o `symbol`, o
  `expected_hash` que o `brain_read` imprime no cabeçalho e o `new_body` (o bloco
  `{ … }` completo). Não reproduza a âncora literal do `Edit` nem releia o arquivo
  inteiro para editar. Se o hash divergir, o arquivo mudou: releia o símbolo e reenvie
  o hash atual — a ferramenta nunca escreve sobre uma versão que você não viu.
- Arquivo inteiro (config, doc, arquivo novo) usa `brain_write`; diff unificado usa
  `brain_patch`; terminal (git, formatador) usa `brain_shell`, nunca `shell` nativo.
- Se a busca não respondeu, refine a busca (outra keyword, `max_results` maior,
  `brain_context_graph`) — não abra o arquivo.

### Encerramento

Cite `path:linhas` dos resultados do brain-tools. Se abriu um arquivo com `Read`, diga
por que o brain-tools não bastou.
```

## 2. Hook — `~/.claude/settings.json`

`scripts/brain-read-once.py` recusa a releitura de uma faixa de linhas já lida na
sessão, quando o arquivo não mudou desde então.

```json
{
  "permissions": {
    "allow": [
      "mcp__brain-tools__brain_search",
      "mcp__brain-tools__brain_context_graph",
      "mcp__brain-tools__brain_impact_check",
      "mcp__brain-tools__brain_peek",
      "mcp__brain-tools__brain_exec",
      "mcp__brain-tools__brain_shell",
      "mcp__brain-tools__brain_read",
      "mcp__brain-tools__brain_edit",
      "mcp__brain-tools__brain_write",
      "mcp__brain-tools__brain_patch",
      "mcp__brain-tools__brain_map",
      "mcp__brain-tools__brain_assert",
      "mcp__brain-tools__brain_diff",
      "mcp__brain-tools__brain_why",
      "mcp__brain-tools__brain_ledger"
    ]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "python3 -I /home/desktop/Documents/projects/brain_tools/scripts/brain-read-once.py",
            "timeout": 10
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "python3 -I /home/desktop/Documents/projects/brain_tools/scripts/brain-read-once.py",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

### Por que ele não trava a edição

O `Edit` exige um `Read` prévio na sessão. Se o hook bloqueasse todo `Read`, editar
ficaria impossível. Por isso a regra é só "não repita":

| Situação | Resultado |
|---|---|
| Primeira leitura de um arquivo | passa (é ela que habilita o `Edit`) |
| Releitura coberta por leitura anterior, arquivo inalterado | **bloqueada** |
| Faixa nova (`offset` fora do que já foi lido) | passa |
| Arquivo mudou no disco desde a última leitura | passa |
| Payload inválido / arquivo inexistente | passa (fail-open) |

O `PostToolUse` registra as faixas lidas em `~/.claude/state/read-once-<sessão>.json`;
o `PreToolUse` consulta. Estado por sessão — duas sessões não interferem.

## Verificação

```bash
BRAIN_READ_ONCE_STATE_DIR=$(mktemp -d) python3 -I scripts/brain-read-once.py <<'JSON'
{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Read","tool_input":{"file_path":"/tmp/x.rs"}}
JSON
# depois repita com "PreToolUse": deve sair com código 2 e a mensagem no stderr
```

## Edição e o hook

`brain_edit` é uma tool MCP, não o `Edit` nativo: o gate read-before-edit do harness
não se aplica a ela. A garantia equivalente é o `expected_hash` — e, como regras
`Read(path)` de `permissions.deny` só valem para ferramentas nativas, restringir
`brain_edit` exige um deny em `mcp__brain-tools__brain_edit` ou um `PreToolUse` com
matcher na tool. O hook de releitura continua útil, mas deixa de ser pré-requisito
para editar.

## Limite conhecido

O hook impede a releitura **do mesmo arquivo**. Ele não sabe que um `Read` ocorreu
porque o `brain_search` já devolveu aquele trecho — isso exigiria o servidor MCP
registrar as faixas servidas para o hook consultar. Enquanto isso, esse caso fica com
a camada de instrução.
