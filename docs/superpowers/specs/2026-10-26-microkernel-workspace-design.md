# Microkernel por Workspace de Crates

**Data:** 2026-10-26
**Status:** Aprovado para implementação

## Contexto

O `brain-tools` hoje é um crate único de ~2400 linhas com seis módulos internos
(`config`, `engine`, `error`, `graph`, `mcp`, `search`). A intenção é uma arquitetura de
microkernel: um núcleo mínimo e estável, com cada ferramenta do MCP (`brain_search`,
`brain_context_graph`, `brain_impact_check`) como um plugin registrado nele.

Duas descobertas do levantamento moldaram este desenho:

1. **A costura já existe, mas não é plugável.** `src/mcp/server.rs` define
   `trait ToolDispatcher`, porém `BrainToolDispatcher` guarda `SearchHandler` e
   `GraphHandler` como campos concretos, resolve as ferramentas por um `match` fixo de
   nomes e hardcoda os três schemas em `list_tools()`. É um trait com um único
   implementador, não um registro.

2. **`search::ast_chunker` não é plugin.** `graph/builder.rs` e `graph/queries.rs`
   dependem de `SupportedLanguage`, e `engine/` depende de `AstSnippet`. Se o chunker
   ficasse no crate da ferramenta de busca, o grafo e o motor passariam a depender de um
   plugin — invertendo a hierarquia. Logo, o parser AST e os tipos compartilhados sobem
   para o kernel; o que desce para os plugins é apenas a *ferramenta* (handler + schema).

O escopo é estritamente estrutural: **nenhum comportamento novo**. É movimentação de
código, ajuste de imports e troca do despacho `match` por um registro.

## Decisão: workspace, não carregamento dinâmico

Plugins são **crates ligados estaticamente**. Fronteiras impostas pelo Cargo, não por
`dlopen`. Motivo: com 3 ferramentas e ~2400 linhas, ABI estável, versionamento de
protocolo e tratamento de panic entre fronteiras FFI custariam mais código de
infraestrutura do que o projeto tem hoje. A fronteira que importa — "o kernel não conhece
as ferramentas" — o Cargo garante.

## Arquitetura

```
brain-kernel      → nenhuma dependência interna
   ├─ protocol     (JSON-RPC, Tool, CallToolResult)
   ├─ types        (AstSnippet, ScoredSnippet, SupportedLanguage)
   ├─ ast          (AstChunker)
   ├─ registry     (trait ToolPlugin, Registry)
   └─ config, error

brain-search      → brain-kernel     (scanner, ranker, SearchTool)
brain-graph       → brain-kernel     (builder, model, queries, traversal, watcher, 2 tools)
brain-engine      → brain-kernel     (llama_validator, prompt, actor)

brain-tools       → todos            (binário + re-exports de compatibilidade)
```

Grafo acíclico. Nenhum plugin depende de outro plugin.

### Contrato de plugin

```rust
pub trait ToolPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> Tool;
    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>;
}
```

`Registry` guarda `HashMap<&'static str, Arc<dyn ToolPlugin>>` e é o único implementador de
`ToolDispatcher`: `list_tools()` agrega os schemas registrados, `call_tool()` faz lookup
por nome. Adicionar uma ferramenta passa a ser: novo tipo no crate dela + uma linha de
`register` no binário — sem editar `match` nem lista de schemas.

O binário monta o registro em `main.rs`, mantendo a ordem de inicialização atual
(modelo → handlers → watcher).

### Propagação de features

`llama-cpp-2` fica isolado em `brain-engine`. As features `cuda`, `vulkan` e `metal` são
declaradas no workspace e propagadas: `brain-tools/cuda → brain-engine/cuda →
llama-cpp-2/cuda`. Um erro aqui quebraria apenas a variante GPU, que é a usada em
produção — por isso a verificação final inclui build com `--features cuda`.

## Compatibilidade

Os 11 testes de integração importam caminhos como `brain_tools::search::scanner` e
`brain_tools::mcp::protocol`. O crate `brain-tools` mantém módulos de re-export
(`pub use brain_search as search;`) preservando todos os caminhos atuais, tornando a
migração incremental e reversível.

`brain_tools::Config` e `brain_tools::BrainError` continuam exportados na raiz.

## Arquivos

**Novos:** `Cargo.toml` (workspace), `crates/brain-kernel/`, `crates/brain-search/`,
`crates/brain-graph/`, `crates/brain-engine/`, `crates/brain-tools/`.

**Movidos:** os 23 arquivos de `src/`, distribuídos por responsabilidade, preservando
`git` history via `git mv`.

**Alterados:** `src/mcp/server.rs` → despacho por registro; `src/mcp/handlers.rs` →
handlers viram `ToolPlugin`; `src/main.rs` → monta o registro.

## Testes

- Os 11 testes de integração existentes devem continuar passando sem alteração de
  import, comprovando a compatibilidade.
- 3 testes unitários novos no kernel: registro vazio, registro duplicado (deve falhar),
  lookup por nome inexistente.
- `cargo test` completo verde em ambos os perfis (padrão e `--features cuda`).
- Verificação end-to-end: `initialize` + `tools/call` com o binário final, no cenário que
  falhava antes (busca por fome em `dreamstrategygame`).

## Fora de escopo

Filtro de arquivos de teste e o tool `brain_list_files` — ambos aprovados em conversa
anterior, mas pertencem a uma mudança de comportamento, não a esta reorganização. Devem
ser implementados **depois**, sobre a estrutura nova.
