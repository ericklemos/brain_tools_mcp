# `brain_edit` — Edição por Span AST

**Data:** 2026-10-26
**Status:** Proposto — aguardando revisão
**Programa:** frente S3 (independente do ranqueador `d3`)

## Contexto

O ciclo de edição é o maior custo de tokens que o `brain-tools` ainda não toca. Toda
edição de um arquivo que o agente já viu custa hoje:

1. um `Read` do arquivo **inteiro**, exigido pelo harness antes do `Edit`;
2. um `Edit` com `old_string` — âncora literal que precisa casar exatamente uma vez.

Para uma função de 40 linhas dentro de um arquivo de 600, o passo 1 é 15× o conteúdo
que interessa, e o passo 2 obriga o agente a reproduzir texto que ele acabou de ler. O
`brain_read` já resolve o "ver o que tem" (esqueleto ou corpo de um símbolo), mas não o
"não precisar ver para escrever".

A premissa do `brain_edit` é que o daemon, que já sabe onde um símbolo começa e termina
na AST, faça o splice por ele: o agente manda **nome do símbolo + corpo novo**, nunca
âncora de texto nem arquivo inteiro.

## Levantamento que moldou o desenho

**1. `AstSnippet` não serve como fonte da edição.** Ele carrega `start_line`/`end_line`,
não offsets em bytes (`crates/brain-kernel/src/ast.rs:22-30`), e seu `code` é **truncado
em 80 linhas** (`ast.rs:94-99`). Edificar sobre ele significaria editar texto incompleto
em qualquer função longa. Além disso, `end_line` é `end_position().row + 1`
(`ast.rs:106-107`), que pode marcar uma linha a mais que o fim real do símbolo —
inofensivo na leitura, fatal numa escrita. A edição precisa de um resolvedor próprio,
com `start_byte`/`end_byte`.

**2. O harness não exige `Read` antes de escrita feita por MCP.** O gate
"read-before-edit" é comportamento das ferramentas nativas `Edit`/`Write`, não do
pipeline de aprovação de MCP. Verificado empiricamente contra o próprio Claude Code: um
servidor MCP stdio cuja tool escreve direto no disco executou sem leitura prévia, e o
controle nativo (`Write` no mesmo diretório, sem `Read`) falhou com
`File has not been read yet. Read it first before writing to it.` A documentação define
o check dentro de "Edit tool behavior" e "Write tool behavior"
(https://code.claude.com/docs/en/tools-reference.md#edit-tool-behavior), e trata MCP
como gate de servidor/regra por nome de tool, não como inspeção de conteúdo.

Consequência direta: **a premissa se sustenta**, mas a troca é real e é de segurança.

**3. Nenhum guardrail nativo se aplica a nós.** O harness não consegue aplicar a uma
tool MCP o que ele aplica ao `Edit`/`Write`:

| Guardrail nativo | Situação no `brain_edit` |
|---|---|
| read-before-edit | substituído por `expected_hash` (decisão deste desenho) |
| `old_string` casa exatamente uma vez | substituído pela resolução de span na AST |
| recusa de escrita através de symlink | reimplementado no servidor (§2) |
| recusa de arquivo não-UTF-8 | reimplementado no servidor (§2) |
| regras `Read(path)` de deny também bloqueiam `Edit`/`Write` | **não existe** para MCP — deny de caminho é das tools nativas |

Os três últimos precisam virar requisito explícito, não suposição herdada.

## Objetivo

Editar o corpo de um símbolo existente por nome, com garantia de que o agente está
editando sobre uma versão do arquivo que ele realmente viu, e devolver o resultado lido
do disco com o impacto da mudança — dentro do contrato de orçamento que as outras
ferramentas já seguem.

## Não-objetivos

- Sem criar arquivos, renomear, mover ou apagar símbolos.
- Sem edição multi-símbolo numa chamada.
- Sem formatação automática (`rustfmt`/`ktlint`) — o daemon escreve exatamente os bytes
  recebidos. Formatar é `brain_exec` num segundo turno, com resultado verificável.
- Sem Kotlin na primeira entrega. O chunker cobre `.kt`, mas as âncoras, o idioma e os
  testes mudam; Kotlin entra depois de Rust estar estável, com a mesma interface.
- Sem edição cirúrgica de linha (`old_text`/`new_text` dentro do símbolo). Se o uso
  mostrar que reescrever o corpo inteiro é caro demais, isso vira uma ferramenta
  própria, não um segundo modo aqui.
- Sem controle de concorrência entre sessões além do hash. Duas sessões editando o mesmo
  arquivo: a segunda recebe o mismatch e decide.

## Desenho

### 1. Interface

```jsonc
// nome da tool
"brain_edit"

// input_schema
{
  "type": "object",
  "properties": {
    "path":          { "type": "string" },
    "symbol":        { "type": "string" },
    "expected_hash": { "type": "string" },
    "new_body":      { "type": "string" },
    "signature":     { "type": "string" },
    "budget_tokens": { "type": "integer" }
  },
  "required": ["path", "symbol", "expected_hash", "new_body"]
}
```

- `path` — relativo ao `--root-dir`, como em `brain_read`.
- `symbol` — nome simples (`verify_token`) **ou** qualificado pelo container
  (`AuthService::verify_token`).
- `expected_hash` — blake3 (hex) do arquivo inteiro, do jeito que `brain_read` passa a
  imprimi-lo.
- `new_body` — substitui o bloco `{ … }` **inteiro, chaves incluídas**. O splice é de
  bytes crus: sem reindentação, sem reformatar, sem tocar no que vem antes ou depois.
- `signature` — opcional; se presente, substitui a parte do item que vem antes do `{`
  (assinatura, genéricos, `where`). Sem ele, a assinatura fica como está.
- `budget_tokens` — o mesmo contrato das outras ferramentas.

**Nota sobre `annotations` (MCP):** a tool quer declarar `readOnlyHint: false` e
`idempotentHint: true`, para que o cliente possa repetir a chamada com segurança após
uma conexão fechada. Hoje o `Tool` do kernel **não tem** campo de annotations
(`crates/brain-kernel/src/protocol.rs:99-104`): só `name`, `description` e
`inputSchema`. Adicionar o campo é opcional e separado desta spec — se for feito, é
`Option<serde_json::Value>` com `skip_serializing_if = "Option::is_none"`, para que o
JSON de `tools/list` das outras ferramentas fique byte-idêntico. Sem ele, a ferramenta
funciona; só perde a dica de retry junto ao cliente.

### 2. Resolvedor de span (`crates/brain-graph/src/span.rs`)

```rust
pub struct SymbolSpan {
    pub name: String,              // nome do símbolo, como achado na AST
    pub container: Option<String>, // `impl AuthService` / `mod tests`, se houver
    pub head_range: Range<usize>,  // bytes: começo do item até o `{` do corpo
    pub body_range: Range<usize>,  // bytes: `{` do corpo até o `}` inclusive
    pub start_line: usize,         // 1-based, derivado de body_range
    pub end_line: usize,
}

pub struct SpanResolver;

impl SpanResolver {
    /// Todos os itens nomeados do arquivo, com ranges em bytes.
    pub fn symbols(&self, path: &Path, content: &str) -> Result<Vec<SymbolSpan>>;
    /// Resolve `symbol` (simples ou qualificado) ou devolve ambiguidade com candidatos.
    pub fn resolve(&self, symbols: &[SymbolSpan], symbol: &str) -> ResolveOutcome;
}

pub enum ResolveOutcome {
    Found(SymbolSpan),
    Ambiguous(Vec<SymbolSpan>),
    NotFound { suggestion: Option<String> }, // sugestão por distância de edição
    NoBody { candidates: Vec<String> },      // `fn` de trait, item sem corpo
}
```

Regras de resolução:

1. **Parsing** com o mesmo `tree_sitter` e os mesmos grammars de `ast.rs`, cobrindo
   `function_item`, `impl_item`, `struct_item`, `enum_item`, `trait_item`, `mod_item`
   (Rust) e os equivalentes Kotlin. A diferença em relação ao `AstChunker` é dupla:
   uso de `start_byte`/`end_byte` e **descida recursiva** — o chunker atual só *sobe*
   até o escopo mais próximo (`ast.rs:116-145`), então um método dentro de um `impl`
   seria devolvido como o `impl` inteiro se fosse reaproveitado.
2. **Corpo** é o `body` do item — `field("body")` quando existe; para itens sem corpo
   (trait `fn`, `struct` unitária), `NoBody` com os nomes candidatos. Nunca "editar o
   item inteiro" por inferência.
3. **Qualificação**: nome simples casa contra `name`; `Container::nome` casa contra
   `name` **e** `container`. Um nome simples que casa em mais de um lugar devolve
   `Ambiguous` com a lista (`impl Foo::new — linha 12`, `impl Bar::new — linha 40`) e o
   agente reenvia qualificado. **Nunca escolhe o primeiro** — é a mesma política de
   `scope::pick_target`, que trata adivinhação como dano ao grafo.
4. **Faixas de linha** são derivadas de `body_range`, não de `end_position().row + 1`.
   Isso também corrige o `end_line` otimista do chunker para quem passar a usar
   `SymbolSpan`.

O `SpanResolver` é independente do grafo: trabalha sobre o conteúdo do arquivo que ele
mesmo lê. Não consulta nem muta `ProjectCodeGraph`.

### 3. Módulo da ferramenta (`crates/brain-graph/src/edit_tool.rs`)

```rust
pub struct EditTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>, // só para o relatório de impacto
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}
```

Fluxo de `call`:

1. **Resolver o caminho** contra o `root_dir` — canonicalização e recusa de traversal,
   idêntico a `ReadTool::resolve` (`read_tool.rs:90-100`). Recusa adicionalmente se o
   caminho **canonicalizado** for symlink para fora do root.
2. **Ler o arquivo** como bytes; recusar se não for UTF-8 válido.
3. **Hash**: `blake3(content)` comparado a `expected_hash` (aceita hex completo ou o
   prefixo de 12 caracteres que o `brain_read` imprime).
   - Igual → segue.
   - Diferente mas **já aplicado** → responde `já aplicada` (ver §5, retry).
   - Diferente → `CallToolResult::error` com o hash atual, sem escrever.
4. **Resolver o span** com `SpanResolver`. `Ambiguous`/`NotFound`/`NoBody` →
   `CallToolResult::error` com os candidatos. Sem escrever.
5. **Montar o novo conteúdo em memória**: `content[..body_range.start] +
   new_body + content[body_range.end..]`, e, se `signature` veio,
   `content[..head_range.start] + signature + " " + content[body_range.start..]`
   — a assinatura substitui tudo o que antecede o `{`, preservando o corpo já trocado.
6. **Re-parse** do resultado. Se não parsear (sintaxe inválida), **recusa e não
   escreve**: o daemon não deixa o repositório num estado que ele mesmo não consegue
   indexar.
7. **Escrita atômica**: arquivo temporário no **mesmo diretório** (mesmo filesystem,
   senão o `rename` deixa de ser atômico), escreve, `fsync`, `rename` sobre o original.
   Preserva as permissões do original (`std::fs::metadata` → `set_permissions`).
8. **Verificação pós-escrita** — relê o disco e confirma os três fatos:
   a. o blake3 do que está em disco é o do conteúdo computado em memória;
   b. o símbolo re-resolve no arquivo novo;
   c. o corpo re-resolvido é igual byte a byte ao trecho correspondente do conteúdo
      computado.
   Falha em qualquer um → `CallToolResult::error` descrevendo qual falhou. Essa etapa
   é o que impede a ferramenta de afirmar sucesso por bug próprio.
9. **Reindexação**: o `GraphWatcher` já detecta a mudança e reindexa só esse arquivo
   (`watcher.rs:46-60`, deduplicado por blake3 em `builder.rs:101-111`). A ferramenta
   **não** sincroniza o grafo à mão; o invariante é "conteúdo manda", e a releitura em
   (8) já acontece depois do `rename`. O relatório de impacto lê o grafo como ele
   estiver — que é, no pior caso, o estado de um instante antes.
10. **Montar a resposta** dentro do orçamento (§5).

### 4. Concorrência e idempotência

- **Duas sessões, mesma âncora**: a primeira vence; a segunda recebe o hash atual e
  decide. É o mesmo contrato do `Edit` nativo (que falha se `old_string` não casar), só
  que expresso em hash de arquivo em vez de texto.
- **Retry após conexão fechada** (`idempotentHint: true`): se a escrita aplicou mas a
  resposta se perdeu, o retry manda o `expected_hash` antigo. Recusar seria seguro e
  inútil — empurraria o agente a reler o arquivo para descobrir que já está como
  queria. Por isso, no mismatch, a ferramenta resolve o símbolo e compara o corpo atual
  com `new_body` (desconsiderando espaços nas pontas): se forem iguais, responde
  `já aplicada` **sem escrever**, com o hash atual.
- **Watcher durante a escrita**: o `rename` é atômico, então o watcher nunca vê um
  arquivo meio escrito — vê o original ou o novo.

### 5. Retorno

Sucesso — uma seção, prioridade 255:

```markdown
### `AuthService::verify_token` — `auth.rs:14-31` editado

```rust
<corpo novo, verbatim, lido do disco>
```

Arquivo: blake3 `b91c3a1f77e2` (antes `a3f1d09c4b22`)
Impacto: 2 chamadores (`LoginHandler::call`, `middleware.rs:88`), 1 teste que o alcança (`tests/auth_test.rs:12`)
```

- O corpo vem **do disco relido** em §3(8), não do argumento — o agente vê o que ficou
  de fato.
- Se o corpo exceder o orçamento, ele vai para o `ResponseStore` e vira handle
  `h_xxxxxxxx`, com a seção reduzida ao cabeçalho + hash + impacto.
- **Impacto**: contagem de arestas de entrada no grafo para o símbolo (chamadores fora
  do arquivo editado, como no `brain_diff`) e, dentre eles, os que estão sob `#[cfg(test)]`
  ou em arquivo de teste — ou seja, **quem alcança** o símbolo, não "testes por
  proximidade". É leitura do grafo, não varredura nova. Se o grafo ainda não reindexou
  (ver risco correspondente), a contagem pode não incluir arestas novas criadas pela
  própria edição; chamadores externos não desaparecem por isso.
- **Já aplicada** (retry): resposta de duas linhas, sem corpo, sem reescrita.

Falhas — sempre `CallToolResult::error`, nunca texto que pareça sucesso:

| Situação | Mensagem |
|---|---|
| hash divergente | `Arquivo mudou desde a leitura. Hash atual: <novo>. Reenvie com expected_hash atualizado.` |
| símbolo ambíguo | `Símbolo `new` aparece 2 vezes: impl Foo::new (linha 12), impl Bar::new (linha 40). Reenvie qualificado.` |
| símbolo ausente | `Símbolo `X` não encontrado em `path`. Mais próximo: `Y`.` |
| item sem corpo | `Símbolo `X` não tem bloco `{…}` (fn de trait).` |
| corpo novo inválido | `O arquivo não parsearia depois da edição (erro na linha N). Nada foi escrito.` |
| fora do root / symlink / não-UTF-8 | recusa explícita citando qual |

### 6. Mudança no `brain_read` (única em ferramenta existente)

O cabeçalho do esqueleto e da seção de símbolo passam a trazer o hash do arquivo:

```
### `auth.rs` — 612 linhas · blake3 `a3f1d09c4b22`
```

12 caracteres hex, o suficiente para o `expected_hash` e curto o bastante para não
pesar no contexto. A comparação aceita esse prefixo; o hash completo aparece no retorno
do `brain_edit`.

Também no `brain_read`, o `end_line` de cada símbolo passa a vir de `body_range` (via
`SpanResolver`), corrigindo o `+1` otimista. O `AstChunker` continua como está para a
busca — mudar o chunker propagaria a correção para `brain_search`, que é outro escopo.

### 7. Documentação de instrução

`docs/hooks.md` e o bloco de `~/.claude/CLAUDE.md` ganham uma frase: editar símbolo
existente usa `brain_edit` com o hash do `brain_read`, e o `Read` só é legítimo quando o
símbolo **ainda não foi visto** — depois da primeira leitura, `brain_edit` substitui
`Read` + `Edit`.

## Arquivos

**Novos:**
- `crates/brain-graph/src/span.rs` — `SpanResolver`, `SymbolSpan`, `ResolveOutcome`.
- `crates/brain-graph/src/edit_tool.rs` — `EditTool` + testes.

**Alterados:**
- `crates/brain-graph/src/read_tool.rs` — hash no cabeçalho; `end_line` por `body_range`.
- `crates/brain-graph/src/lib.rs` — exporta os módulos novos.
- `crates/brain-tools/src/main.rs` — registra `EditTool` (após `ReadTool`, para preservar
  a ordem observável de `tools/list`).
- `crates/brain-tools/tests/daemon_smoke_test.rs` — contagem de tools.
- `README.md` — item na lista de funcionalidades; `docs/hooks.md` — instrução de uso.
- `docs/guia_de_execucao_e_integracao_mcp.md` — tool mencionada na verificação.

## Testes

**Unidade — `span.rs`**

- função de topo: `body_range` cobre exatamente `{ … }`;
- método dentro de `impl`: o span é do método, não do `impl` (a descida recursiva);
- `impl` com vários métodos: editar um não move os outros;
- nome simples que casa em dois `impl`s → `Ambiguous` com ambos;
- `Tipo::metodo` qualificado → resolve o certo;
- `fn` de trait terminando em `;` → `NoBody`;
- `struct` unitária e `struct` com corpo;
- símbolo ausente → `NotFound` com sugestão mais próxima;
- `end_line` derivado de `body_range` nunca passa do `}`;
- CRLF: os offsets não contam `\r` como nova linha.

**Unidade — `edit_tool.rs`**

- edição feliz: bytes do corpo trocados, resto do arquivo byte a byte igual;
- `signature` presente troca só a cabeça, corpo preservado;
- hash divergente → erro, arquivo intocado;
- hash divergente **com corpo já igual** → `já aplicada`, arquivo intocado;
- `new_body` que não parseia → erro, arquivo intocado (assert no conteúdo original);
- símbolo ambíguo → erro listando candidatos, arquivo intocado;
- path `../../etc/passwd` → erro;
- arquivo com symlink para fora do root → erro;
- arquivo não-UTF-8 → erro;
- `new_body` maior que o orçamento → envelope com handle, e `brain_peek` recupera.

**Integração (binário real)**

- `brain_read` → hash → `brain_edit` com esse hash → `brain_read` de novo devolve o
  corpo novo e o hash novo;
- depois do `brain_edit`, `brain_impact_check` no símbolo reflete o arquivo novo (prova
  que o watcher reindexou, ou que o grafo não ficou preso ao conteúdo antigo);
- `tools/list` inclui `brain_edit`.

**Sensibilidade por mutação** (o padrão já usado em `response_contract_test.rs`): trocar
a comparação de hash por `true` deve fazer falhar o teste de hash divergente; remover o
re-parse deve fazer falhar o teste de corpo inválido. Um teste que passa com a
verificação desligada não está medindo a verificação.

## Decisões e riscos

- **Hash obrigatório é mais rígido que o necessário.** Qualquer mudança no arquivo —
  inclusive em código que o agente não tocou — invalida a edição. É deliberado: editar
  sobre um arquivo que mudou é exatamente como edições se perdem, e recusar é
  recuperável em um turno (reenviar o hash novo). O custo é um erro a mais em editores
  que salvam sozinhos (format-on-save) entre a leitura e a escrita.
- **`new_body` com chaves é redundante na sintaxe, mas não no contrato.** O agente manda
  o bloco `{ … }` completo, o que permite edições que mudam a estrutura do corpo sem
  inventar regra de "miolo indentado". A contrapartida é que um `new_body` sem `{` é
  erro do agente, detectado no re-parse (§3.6).
- **Sem formatação automática.** `rustfmt` num arquivo inteiro reformata código que o
  agente não pediu para mudar, transformando uma edição de 10 linhas num diff de 300.
  Se o código novo não casar com o estilo, o próximo `brain_exec fmt` resolve, e esse é
  um passo visível.
- **Sem Kotlin inicialmente.** A interface é agnóstica o suficiente para aceitar Kotlin
  sem mudança de schema; o que falta são as regras de corpo (`property_declaration` com
  inicializador não tem `{ }`) e os testes. Entregar Rust primeiro evita prometer
  paridade que não foi exercitada.
- **O relatório de impacto pode estar um instante atrasado.** O grafo é reindexado pelo
  watcher de forma assíncrona. O impacto lido logo após o `rename` reflete o estado
  anterior para *novas* arestas que a edição criou. Aceitável: a contagem de chamadores
  externos quase nunca muda com uma edição de corpo, e um segundo `brain_impact_check`
  sempre pode ser pedido.
- **Deny rules de caminho não protegem esta tool.** Regras `Read(path)` de
  `permissions.deny` valem para as ferramentas nativas. Quem quiser impedir o
  `brain_edit` de tocar certos diretórios precisa de `permissions.deny` com
  `mcp__brain-tools__brain_edit`, ou de um hook `PreToolUse` com matcher na tool. A
  spec documenta isso; não tenta reimplementar política de segurança do usuário.
- **Escrita concorrente entre sessões não é bloqueada.** Duas sessões que leem o mesmo
  hash e escrevem em sequência: a primeira aplica, a segunda recebe mismatch. Não há
  lock de arquivo — o custo de um lock atravessando sessões (e processos MCP distintos)
  não se paga para uma ferramenta de uso interativo.

## Critério de sucesso

1. Editar um símbolo já visto custa os bytes de `new_body` e nada de `Read` — medido
   num arquivo de 600 linhas, entrada ≤ 20 tokens além do corpo novo.
2. O agente nunca recebe "feito" sem que o disco tenha sido relido e conferido (§3.8).
3. Arquivar um erro é sempre recuperável em um turno: ambiguidade dá candidatos, hash
   dá o valor atual, sintaxe inválida não escreve nada.
4. Nenhuma ferramenta existente muda de comportamento além do cabeçalho do `brain_read`
   e da correção do `end_line`.
5. Um retry depois de resposta perdida não reescreve nem confunde o agente.
