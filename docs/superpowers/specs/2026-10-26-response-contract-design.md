# Contrato de Retorno com Orçamento e Paginação

**Data:** 2026-10-26
**Status:** Proposto — aguardando revisão
**Sub-projeto:** A de A–E (contrato de retorno)

## Contexto

Hoje nenhuma ferramenta do `brain-tools` tem noção de tamanho de resposta. Os três
handlers acumulam tudo num `String` e devolvem via `CallToolResult::text`:

- `SearchHandler::handle_brain_search` (`crates/brain-search/src/search_tool.rs:113-152`)
  concatena **todos** os blocos aprovados pelo validador. O `max_results` limita a
  quantidade de blocos, não o custo em tokens — cinco funções de 80 linhas já passam de
  qualquer orçamento razoável.
- `GraphTraversal::context_graph` e `impact_check`
  (`crates/brain-graph/src/traversal.rs:39+`) iteram o grafo e concatenam cada aresta.
  Em símbolos centrais o relatório cresce sem teto.

O resultado é o vazamento que este sub-projeto ataca: **a resposta inteira entra no
contexto do agente, sempre**, mesmo quando ele precisaria de 10% dela.

Duas descobertas do levantamento moldaram o desenho:

1. **O registro já existe.** `brain_kernel::plugin::Registry` e o trait `ToolPlugin` já
   são usados pelas três ferramentas (`crates/brain-kernel/src/plugin.rs`,
   `crates/brain-graph/src/graph_tool.rs`). O contrato não precisa mexer neles.
2. **O corte precisa ser por seção, não por caractere.** Cortar uma resposta de grafo no
   meio de uma lista de chamadores produz uma resposta que parece completa e engana o
   agente. A unidade de corte tem que ser semanticamente inteira.

## Objetivo

Toda ferramenta responde dentro de um orçamento de tokens declarado, e o excedente fica
recuperável por handle em vez de entrar inteiro no contexto.

## Não-objetivos

- Nenhuma ferramenta nova além de `brain_peek`, que é o par do envelope.
- Sem tokenizer real — a estimativa é por caractere (ver Decisões).
- Sem cache semântico, sem deduplicação entre ferramentas, sem compressão de texto.
- Sem mudança em `Registry` ou no trait `ToolPlugin`.
- Sem persistência do handle em disco.

## Desenho

### 1. Unidade de conteúdo: `Section`

Um módulo novo no kernel, `crates/brain-kernel/src/budget.rs`:

```rust
pub struct Section {
    pub heading: String,  // ex: "`verify_token` (`src/auth.rs:3-8`) — 92%"
    pub body: String,     // markdown da seção, atômico
    pub priority: u8,     // 0 = descartável, 255 = nunca cortar
}
```

Cada ferramenta passa a montar `Vec<Section>` em vez de um `String`. A renderização —
ordem, corte, envelope — é do kernel, não da ferramenta. Isso é o que impede as oito
ferramentas planejadas de inventarem oito formatos de truncamento.

### 2. Orçamento

- Toda ferramenta aceita `budget_tokens: Option<usize>` no schema.
- Default `2000`, configurável por `--default-budget-tokens` / `BRAIN_BUDGET_TOKENS`,
  ao lado de `--default-max-results` em `crates/brain-kernel/src/config.rs`.
- Estimativa: `ceil((heading.chars().count() + body.chars().count()) / 3.5)`. Sem
  tokenizer, sem dependência nova.
- **Seleção:** ordena as seções por `priority` decrescente e inclui seções inteiras
  enquanto couberem. Se a seção de maior prioridade já excede o orçamento sozinha, ela
  entra mesmo assim — nunca se perde a resposta principal por causa do envelope.
- **Renderização:** as seções incluídas saem na ordem original em que a ferramenta as
  montou, não na ordem de prioridade. Prioridade decide o que entra; ordem decide como
  aparece.

### 3. Envelope

Só aparece quando houve corte. Sem corte, a saída é byte-idêntica à de hoje. Formato,
prefixado ao corpo:

```
> Resposta truncada: 4 seções omitidas (~8200 tokens estimados).
> Recupere com `brain_peek` — handle: `h_9f3a`.

```

O handle é um token estável de 8 hex com prefixo `h_`, fácil de o agente citar e de o
teste casar com regex. Mantém-se texto puro porque o transporte MCP é
`content[0].text`; JSON dentro de texto obrigaria o agente a parsear.

### 4. Handle e `brain_peek`

Módulo `crates/brain-kernel/src/store.rs`:

```rust
pub struct ResponseStore {
    entries: DashMap<String, StoredResponse>,  // id -> seções + instante de inserção
    total_bytes: AtomicUsize,
}
```

- TTL de 30 minutos, verificado na leitura; entrada expirada é removida e a leitura
  falha com erro categórico.
- Teto de 64 MB no total, com evicção da entrada mais antiga.
- Id: `blake3` de `(contador, Instant::now())`, truncado a 8 hex. `blake3` já é
  dependência do grafo; nada novo.

O par do envelope é uma ferramenta própria, `brain_peek`
(`crates/brain-kernel/src/peek_tool.rs`, implementando `ToolPlugin`):

| Parâmetro | Default | Descrição |
|---|---|---|
| `handle` | obrigatório | handle devolvido pelo envelope |
| `offset_chars` | `0` | posição no corpo omitido, para paginar |
| `budget_tokens` | `2000` | tamanho da página |

Devolve o trecho pedido e, ao final, `Restante: N caracteres (~M tokens)`. Handle
inválido ou expirado devolve `CallToolResult::error` — nunca uma resposta vazia que o
agente interpretaria como "não há mais nada".

Paginação lê o **corpo omitido**, que é a concatenação das seções que não couberam, na
ordem original, incluindo os títulos. Toda seção contribui primeiro com seu título, então
um trecho recuperado sempre vem precedido do nome do que ele contém. `offset_chars` avança
pelo corpo omitido inteiro, não por seção.

### 5. Política de prioridade por ferramenta

| Ferramenta | Seção | Prioridade |
|---|---|---|
| `brain_search` | um bloco por snippet | `(confidence * 200.0) as u8` — a ordem de confiança já é a ordem útil |
| `brain_context_graph` | um nó por seção | 255 para o nó central, 200 para `Incoming`, 150 para `Outgoing` |
| `brain_impact_check` | um nó por seção | 255 para impacto direto, 120 para transitivo |
| `brain_peek` | uma página | 255 (nunca corta, o paginador é explícito) |

Impacto direto acima do transitivo é o ponto onde orçamento rende mais: é frequente o
relatório ter 2 chamadores diretos e 40 transitivos, e o agente quase sempre decide com
os 2.

## Arquivos

**Novos:** `crates/brain-kernel/src/budget.rs`, `store.rs`, `peek_tool.rs`.

**Alterados:**
- `crates/brain-search/src/search_tool.rs` → monta `Vec<Section>`; construtor recebe
  `Arc<ResponseStore>`.
- `crates/brain-graph/src/traversal.rs` → devolve seções em vez de `String`.
- `crates/brain-graph/src/graph_tool.rs` → construtor recebe o store; renderização
  passa pelo kernel.
- `crates/brain-search/src/schema.rs`, `crates/brain-graph/src/schema.rs` →
  `budget_tokens` nos três schemas.
- `crates/brain-kernel/src/config.rs` → `default_budget_tokens`.
- `crates/brain-kernel/src/lib.rs` → exporta os módulos novos.
- `crates/brain-tools/src/main.rs` → cria o `ResponseStore`, injeta nos handlers e
  registra `brain_peek`.

## Testes

- `budget`: seção que cabe não é cortada; corte respeita prioridade; a primeira seção
  entra mesmo acima do orçamento; ordem de renderização é a original.
- `budget`: nenhuma seção é emitida pela metade.
- `store`: handle expirado devolve erro; evicção por teto de bytes remove a mais antiga.
- `budget`: `priority_ties_keep_tool_order` — com prioridades iguais, a ordem de entrada
  é a que decide quem entra.
- `peek_tool`: paginação por `offset_chars` converge — duas chamadas cobrem o corpo
  inteiro sem sobreposição nem lacuna.
- **Compatibilidade:** com `budget_tokens` alto, as saídas das três ferramentas atuais
  são idênticas às de hoje; os testes de integração existentes continuam verdes com os
  mesmos asserts de conteúdo.
- **Correção ao plano original:** `brain_search_integration_test.rs:68` e
  `graph_tools_test.rs:72` montam o `BrainToolDispatcher` legado, que registra
  deliberadamente só as três ferramentas antigas — não o binário. Continuam afirmando
  `3`. Só `daemon_smoke_test.rs`, que exercita o binário real, passa a afirmar `4`
  incluindo `brain_peek`.
- **Teste E2E no binário** (`response_contract_test.rs`): busca estourada devolve
  envelope; o handle resolve; `brain_peek` devolve o corpo omitido com
  `Restante: 0 caracteres`; handle desconhecido é erro. O teste roda o child dentro do
  diretório-sandbox porque o daemon procura `./models` e um GGUF carregado altera quais
  candidatos sobrevivem.
- **Sensibilidade do E2E verificada por mutação:** subir o orçamento para 100000 faz o
  teste falhar em `expected a truncation envelope`, provando que ele mede o orçamento e
  não passa por acaso.

## Decisões e riscos

- **`chars / 3.5` em vez de tokenizer.** Erra para mais ou para menos conforme o idioma,
  mas o erro não afeta correção — afeta só quantas seções cabem. Um tokenizer real
  custaria uma dependência e latência em toda chamada para refinar um número que já é
  aproximado. Revisitar se o uso mostrar corte agressivo demais.
- **Default 2000 pode truncar respostas que hoje passam inteiras.** É o comportamento
  desejado, mas é uma mudança perceptível. Mitigação: os fixtures dos testes são
  pequenos; o envelope anuncia o corte; e `brain_peek` torna a perda reversível.
- **Página pode partir uma seção.** O corte por seção vale para o que entra no contexto
  do agente; `brain_peek` pagina o corpo omitido por caracteres, então uma seção grande
  pode atravessar duas páginas. Aceitável: é recuperação explícita, e o corpo paginado
  começa com o título da seção.
- **Memória do store.** Teto de 64 MB e TTL de 30 min limitam o pior caso. Um agente em
  loop pode encher o store e causar evicção de handles ainda úteis — aceitável, porque
  o envelope sempre informa o handle e o agente pode refazer a chamada.
- **Seções atômicas podem ser grandes.** Uma única função de 300 linhas é uma seção que
  estoura o orçamento. Entra inteira por decisão (nunca perder a resposta principal) e
  o envelope informa o excesso. Fatiar código no meio seria pior.

## Critério de sucesso

1. Com orçamento folgado, nada muda para o agente — mesmas respostas, mesmos testes.
2. Todo truncamento é anunciado no topo e recuperável por `brain_peek`.
3. Nenhum bloco de código aparece partido.
4. As ferramentas seguintes (B–E) só precisam escolher prioridades para herdar o
   contrato, sem escrever código de truncamento próprio.
