# Integração total com o OpenCode (V2)

No Claude Code a segunda camada é um **hook**. No OpenCode V2 é uma **permissão**:
o harness recusa a chamada antes de o modelo executar. Mesma ideia, mecanismo
diferente — e, para o objetivo "só via MCP", a permissão é mais limpa, porque não
depende de interpretar payload de hook.

| Camada | Onde vive | Garante? |
|---|---|---|
| Instrução | `AGENTS.md` (ou arquivo em `instructions`) | Não — orienta o agente |
| Permissão | `opencode.json(c)` → `permissions` | Sim — o harness nega |
| Política | `opencode.json(c)` → `policies` | Sim — nega até o que a permissão permitiria |

Uma aviso de nomenclatura antes de tudo: **o OpenCode V2 carrega `AGENTS.md`
automaticamente; ele não lê `opencode.md` sozinho.** Este arquivo é o guia. Se você
quiser que o texto de regras se chame `opencode.md`, registre-o em `instructions`
(seção 2).

## O que o brain-tools cobre hoje

| Ferramenta | Substitui | Observação |
|---|---|---|
| `brain_search` | `grep` + abrir arquivo | Trecho verbatim, com `path:start-end` |
| `brain_read` | `read` | Esqueleto ou corpo de símbolo; qualquer texto sob o root |
| `brain_edit` | `edit` por símbolo | Corpo por nome + hash do arquivo; recusa versão divergente |
| `brain_write` | `write` | Arquivo inteiro (configs, docs, arquivos novos) com hash/`overwrite` |
| `brain_patch` | `patch` / `git apply` | Diff unificado: arquivos novos, remoções, trechos fora de símbolos |
| `brain_shell` | `shell` | Comando em `bash -lc` com cwd no root; guarda recusa comandos cobertos (`--shell-guard false` desliga); `--shell-disabled` remove |
| `brain_map` | `glob` + orientação | Só arquivos de código (Rust/Kotlin) |
| `brain_context_graph` / `brain_impact_check` | inspeção manual de chamadas | Quem chama, o que quebra |
| `brain_peek` | paginação de leitura | Recupera o que estourou o budget |
| `brain_exec` | `shell` para build/test | Enum fechado: `build`, `check`, `test`, `clippy` |
| `brain_assert` | grep de confirmação | `exists`, `public`, `used_outside` |
| `brain_diff` | `git diff` + análise | Símbolos alterados e impacto externo |
| `brain_why` | `git log -L` | Histórico do símbolo, sem patch |
| `brain_ledger` | memória de sessão | Grava em `.brain/ledger.jsonl` |

**O ponto que decide o desenho:** a escrita via MCP tem quatro portas, todas no
`main`: `brain_edit` (corpo de símbolo por span, com hash), `brain_write` (arquivo
inteiro, com hash ou `overwrite`), `brain_patch` (`git apply` de um diff) e
`brain_shell` (comando arbitrário, opt-out por flag). `brain_edit` não exige `Read`
prévio: o `expected_hash` é a garantia de versão. Recusa hash divergente, símbolo
ambíguo, item sem corpo e sintaxe inválida; escreve atomicamente e relê o disco
antes de afirmar sucesso.

Consequência prática: negar `edit`/`write`/`patch` do OpenCode e permitir
`brain-tools_*` mantém escrita, criação de arquivo e aplicação de diff via MCP
(`brain_edit`, `brain_write`, `brain_patch`). Negar `shell` e usar `brain_shell`
mantém o terminal via MCP. O que ainda não existe é renomear/mover símbolo com
atualização automática de todas as referências; isso se faz com patch/write.

## 1. Registrar o MCP

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "brain-tools": {
        "type": "local",
        "command": [
          "/caminho/absoluto/brain_tools/target/release/brain-tools",
          "--model-path",
          "/caminho/absoluto/brain_tools/models/vllm-d3-edge-Q8_0.gguf",
          "--gpu-layers",
          "33"
        ],
        "environment": { "RUST_LOG": "info" }
      }
    }
  }
}
```

Não passe `--root-dir`: o OpenCode inicia o servidor com `cwd` no workspace, e o
padrão `.` passa a indexar o projeto aberto. Um registro serve para todos.

Não desligue o Code Mode. Com `codemode: true` (padrão), as ferramentas do MCP
ficam dentro de `execute` e são chamadas como
`tools["brain-tools"].brain_read(...)`. Se você negar `execute`, perde junto todas
as ferramentas de servidores que usam Code Mode — inclusive o brain-tools.

## 2. Instrução — bloco para `AGENTS.md`

O `AGENTS.md` da raiz carrega sozinho. Para usar outro nome de arquivo, aponte-o:

```jsonc
{
  "instructions": ["docs/brain-rules.md"]
}
```

Não aponte `instructions` para este guia inteiro: ele é documentação e viraria
contexto fixo em toda sessão. O bloco útil é curto — a cópia pronta para usar está
em [`brain-rules.md`](brain-rules.md); o texto abaixo é o mesmo, para leitura.

```markdown
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
7. `brain_exec` — build, check, test, clippy. `brain_shell` só para git/formatador/ferramenta externa; a guarda recusa comandos cobertos por outra tool.

Não use `read`, `grep`, `glob` nem `shell` para conferir o que o brain-tools já
entregou. Score baixo ordena candidatas entre si; não mede veracidade. Se a busca
não respondeu, refine a busca — não abra o arquivo.

Editou um símbolo existente? `brain_edit` com o `expected_hash` devolvido por
`brain_read` e o `new_body` completo. Arquivo inteiro ou diff: `brain_write` e
`brain_patch`. Não use `edit`/`write`/`patch` nativos.

Se uma chamada MCP falhar, diga isso explicitamente. Não caia em `grep` em
silêncio: a degradação precisa ser visível.
```

## 3. Permissões — a trava

Três fatos que governam o resto:

- A política base de todo agente é `{"action":"*","resource":"*","effect":"allow"}`.
  Você precisa **negar** o que não quer.
- As regras são avaliadas em ordem e **a última que casa vence**.
- Regras de agente são **anexadas depois** das globais. Elas não substituem; elas
  refinam. Por isso a receita abaixo cabe dentro de um agente próprio.

### Receita extrema — agente `brain-only`

Recomendada porque não arruína o agente `build` do dia a dia. O lockdown vira uma
escolha de sessão, e se o MCP cair você ainda tem um agente normal para consertar.

O arquivo completo, pronto para copiar para a raiz do projeto como `opencode.json`,
está em [`opencode.example.json`](opencode.example.json). O bloco abaixo é o mesmo
conteúdo, para leitura lado a lado com a explicação.

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "instructions": ["docs/brain-rules.md"],
  "agents": {
    "brain-only": {
      "description": "Lê e navega o projeto apenas pelo MCP brain-tools",
      "mode": "primary",
      "permissions": [
        { "action": "read",     "resource": "*", "effect": "deny" },
        { "action": "edit",     "resource": "*", "effect": "deny" },
        { "action": "glob",     "resource": "*", "effect": "deny" },
        { "action": "grep",     "resource": "*", "effect": "deny" },
        { "action": "shell",    "resource": "*", "effect": "deny" },
        { "action": "subagent", "resource": "*", "effect": "deny" },
        { "action": "external_directory", "resource": "*", "effect": "deny" },

        { "action": "execute", "resource": "*", "effect": "allow" },
        { "action": "brain-tools_*", "resource": "*", "effect": "allow" },
        { "action": "opencode_list_mcp_resources", "resource": "*", "effect": "allow" },
        { "action": "opencode_read_mcp_resource", "resource": "brain-tools:*", "effect": "allow" }
      ]
    }
  }
}
```

Para aplicar a **todos** os agentes, mova o array para o `permissions` de topo. É a
mesma lista, sem o envelope `agents`.

### Por que cada negação existe

| Negação | Fecha |
|---|---|
| `read` | Abrir arquivo inteiro, listar diretório |
| `edit` | `edit`, `write` e `patch` — os três compartilham a action `edit` |
| `glob` | Descobrir caminhos sem passar pelo `brain_map` |
| `grep` | Buscar conteúdo sem passar pelo `brain_search` |
| `shell` | `cat`, `sed -i`, `echo >` — **o maior buraco**, porque shell tem a autoridade do usuário |
| `subagent` | Agente filho roda com as **próprias** permissões; `general`/`explore` são portas largas |
| `external_directory` | Leitura/edição fora do workspace |

`execute` **não** é negado: é por ele que as ferramentas do MCP são alcançadas.
Negar `execute` desligaria o Code Mode e, com ele, o brain-tools. Ferramentas
chamadas por dentro do `execute` continuam aplicando as permissões acima — negar
`read` bloqueia `read` mesmo lá dentro.

Negar tool de MCP é por nome normalizado: `brain-tools_brain_edit` bloqueia só a
edição. O curinga `brain-tools_*` libera todas as atuais — inclusive `brain_edit`.

## O que quebra — custos honestos

| Você perde | Por quê |
|---|---|
| Renomear/mover símbolo com refs | `brain_edit` edita corpo, não nome; renomear exige `brain_patch`/`brain_write` e atualizar chamadas |
| `cargo` direto | `shell` negado; use `brain_exec` (enum fechado) ou `brain_shell` |
| Descobrir arquivos não-código | `brain_map` só indexa Rust/Kotlin; `glob` negado. `package.json`, `Dockerfile`, `*.md` só abrem se você souber o nome |
| Imagens, PDFs, binários | `read` do OpenCode faz isso; `brain_read` é texto e recusa fora do root |
| `AGENTS.md` aninhado | A descoberta é disparada por leitura/listagem; sem `read`, só o da raiz carrega |
| Trabalhar fora do workspace | `brain_read` recusa traversal e caminho fora do root |
| Pesquisar ou ler pelo shell | A guarda do `brain_shell` recusa `grep`/`sed`/`cat`/`find`/`ls` e manda usar `brain_search`/`brain_read`/`brain_map`; `--shell-guard false` desliga |

`brain_shell` é poder de shell explícito, não um sandbox. Se "somente leitura" for
uma garantia de segurança e não só de comportamento, suba o servidor com
`--shell-disabled` e `--exec-disabled` — e aceite que o agente não compila, não
testa e não roda git. `brain_exec` também spawna `cargo`, que executa build scripts
com acesso ao filesystem; desligar só `brain_shell` não fecha essa porta.

## Política dura (opcional)

Permissão é por agente e pode ser afrouxada por um `opencode.json` de projeto.
`policies` não: é binária, nunca pergunta, e só aperta. Serve para bloquear algo
que nenhum repo deveria liberar:

```jsonc
{
  "policies": [
    { "action": "tool.use", "resource": "shell:sudo *", "effect": "deny" }
  ]
}
```

## Verificação

Config e MCP recarregam ao salvar; `/reload` força. Não precisa reiniciar.

```bash
opencode mcp list          # brain-tools deve aparecer connected
```

Numa sessão nova, com o agente `brain-only` selecionado:

1. "Leia o `Cargo.toml`." — deve sair por `brain_read`, não por `read`.
2. "Use a ferramenta `read` no `Cargo.toml`." — deve ser **negado** pelo harness.
3. "Rode `cargo test` no shell." — deve ser negado; "rode os testes" deve usar
   `brain_exec`.
4. "Edite o corpo de `verify_token`." — deve sair por `brain_edit`, com o hash do `brain_read`.
5. "Crie um arquivo novo." — deve sair por `brain_write`.
6. "Aplique este diff." — deve sair por `brain_patch`, não por `patch` nativo.
7. "Rode `git status`." — deve sair por `brain_shell`, não por `shell` nativo.

Se o passo 2 passar, a regra não casou: confira se o `permissions` está no agente
certo e se a regra vem **depois** do allow base.

## Recomendação

1. **Não transforme o `build` no agente travado.** Use `brain-only` como agente
   primário separado; o custo de um MCP quebrado deixa de ser um projeto sem
   saída.
2. **As quatro portas de escrita fecham a paridade de edição, não de segurança.**
   `brain_edit`/`brain_write`/`brain_patch` cobrem símbolo, arquivo e diff;
   `brain_shell` cobre o terminal. Nenhuma delas é sandbox: o lockdown é de
   comportamento, não de isolamento.
3. **Se você precisa de imagem, PDF ou descoberta de arquivo não-código, não negue
   `read`/`glob`.** Nesses projetos, a receita certa é instrução + `ask`, não
   `deny`.
