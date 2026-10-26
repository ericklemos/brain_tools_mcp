# Brain-Tools MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Construir um servidor MCP contínuo (daemon stdio) em Rust usando o ecossistema Tokio, que otimiza a busca de código (`brain_search`) e navegação contextual (`brain_context_graph`, `brain_impact_check`) para Rust e Kotlin, eliminando o *token maxxing* através de recortes cirúrgicos de AST e validação local via `llama-cpp-2` com fallback léxico.

**Architecture:** Servidor persistente assíncrono baseado no ecossistema Tokio. O stream JSON-RPC 2.0 opera em `stdin`/`stdout` com `tracing` canalizado exclusivamente para `stderr`. A busca combina varredura léxica com `ignore` e `grep-searcher`, recorte sintático em nós semânticos de Rust e Kotlin via `tree-sitter`, e validação em GPU (CUDA via `llama-cpp-2` em actor thread dedicada) com fallback gracioso para heurística léxica na ausência de GGUF. O grafo de chamadas usa `petgraph::StableDiGraph` mantido em memória e sincronizado incrementalmente por `notify` e `blake3`.

**Tech Stack:**
- **Runtime & Concorrência:** `tokio` (full), `tokio-util`, `dashmap`, `crossbeam-channel`
- **MCP & JSON-RPC:** `serde`, `serde_json`, `tokio::io::AsyncBufReadExt`
- **CLI & Configuração:** `clap` (derive, env)
- **Diagnóstico & Observabilidade:** `tracing`, `tracing-subscriber` (stderr)
- **Erros:** `thiserror`, `anyhow`
- **Busca & Regex:** `ignore`, `grep-searcher`, `grep-regex`
- **AST Parsing:** `tree-sitter`, `tree-sitter-rust`, `tree-sitter-kotlin`
- **Inferência Local:** `llama-cpp-2` (CUDA feature)
- **Grafo & Watcher:** `petgraph` (serde-1), `notify`, `blake3`

**Spec:** `docs/superpowers/designs/2026-10-26-brain-tools-mcp.md` e `arquitetura_e_especifica_o_do_brain_tools.md`

## Global Constraints

- O executável deve ser exclusivamente um servidor MCP stdio (nenhum comando CLI secundário que altere o modo stdio).
- `stdout` é reservado estritamente para mensagens JSON-RPC 2.0; absolutamente nenhum `println!`, `eprintln!` sem formatação ou log deve poluir o `stdout`. Toda instrumentação deve usar `tracing` apontando para `std::io::stderr`.
- Rust (`.rs`) e Kotlin (`.kt`) são linguagens de primeira classe para o `ast_chunker` e o grafo.
- O modo Fallback (sem GGUF) é obrigatório e não deve falhar nem emitir pânico se `--model-path` não for configurado.
- Todas as estruturas do grafo devem usar `petgraph::stable_graph::StableDiGraph` para permitir remoção e reindexação de arquivos sem invalidar índices de nós existentes.

## Review Focus

1. **Pureza de Stdio:** Respostas MCP e erros internos nunca devem quebrar o protocolo JSON-RPC no `stdout`.
2. **Resiliência do Parser AST:** Arquivos de código com erros de sintaxe transitórios (durante digitação) não devem derrubar o servidor nem abortar a busca.
3. **Gerenciamento de Memória / Long-Running:** O servidor roda como daemon; nenhum leak de memória ou handles de arquivos órfãos no watcher incremental.
4. **Respeito ao `.gitignore`:** A busca léxica não deve indexar ou varrer `target/`, `node_modules/`, `.git/`, `build/` ou pastas ignoradas.
5. **Estabilidade de GPU / Fallback:** Caso a inicialização do CUDA ou carregamento do modelo GGUF falhe por falta de VRAM ou arquivo inexistente, o sistema deve logar o erro no `stderr` e degradar graciosamente para o modo fallback sem abortar o processo.

---

## Task Structure

### Task 1: Scaffolding do Projeto Rust e Configuração com Tokio, Clap e Tracing

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/config.rs`
- Create: `src/error.rs`
- Test: `tests/config_test.rs`

- [ ] **Step 1.1: Criar o `Cargo.toml` com todas as dependências do ecossistema**
  Configurar `tokio`, `tokio-util`, `clap`, `tracing`, `tracing-subscriber`, `serde`, `serde_json`, `thiserror`, `anyhow`, `ignore`, `grep-searcher`, `grep-regex`, `tree-sitter`, `tree-sitter-rust`, `tree-sitter-kotlin`, `petgraph`, `notify`, `blake3`, `dashmap` e `llama-cpp-2`.
- [ ] **Step 1.2: Implementar módulo de erros em `src/error.rs`**
  Criar `BrainError` com `thiserror` cobrindo erros de protocolo, I/O, AST parsing, inferência e grafo.
- [ ] **Step 1.3: Implementar parsing de configuração em `src/config.rs`**
  Usar `clap::Parser` com suporte a variáveis de ambiente: `root_dir` (default atual), `model_path` (opcional `Option<PathBuf>`, default para `models/vllm-d3-edge-Q8_0.gguf` se o arquivo existir), `gpu_layers` (default 33), `threads` (default número de CPUs lógicas), `log_level` (default "info"). Incluir flag de execução do script `scripts/download_model.sh`.
- [ ] **Step 1.4: Escrever teste de unidade para `Config` em `tests/config_test.rs`**
  Verificar valores padrão, parsing de flags CLI e variáveis de ambiente.
- [ ] **Step 1.5: Configurar `src/main.rs` com runtime Tokio e `tracing-subscriber` para `stderr`**
  Inicializar `tracing_subscriber::fmt().with_writer(std::io::stderr)` e verificar compilação.
- [ ] **Step 1.6: Executar verificação**
  `cargo test --test config_test` e `cargo check`.

---

### Task 2: Motor de Protocolo MCP e Loop JSON-RPC 2.0 Assíncrono

**Files:**
- Create: `src/mcp/mod.rs`
- Create: `src/mcp/protocol.rs`
- Create: `src/mcp/server.rs`
- Modify: `src/main.rs`
- Test: `tests/mcp_protocol_test.rs`

- [ ] **Step 2.1: Implementar tipos JSON-RPC 2.0 em `src/mcp/protocol.rs`**
  Modelar `JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`, `RequestId`.
- [ ] **Step 2.2: Modelar payloads do protocolo MCP**
  Criar tipos para `initialize` (capabilities, serverInfo: brain-tools v0.1.0), `notifications/initialized`, `tools/list` e `tools/call`.
- [ ] **Step 2.3: Implementar servidor de transporte assíncrono em `src/mcp/server.rs`**
  Usar `tokio::io::BufReader(tokio::io::stdin())` com `lines()` e `tokio::io::stdout()`. Processar requisições concorrentemente sem bloquear o loop de leitura.
- [ ] **Step 2.4: Escrever teste de integração em `tests/mcp_protocol_test.rs`**
  Simular payload JSON-RPC de `initialize` e `tools/list` através de um buffer em memória e validar respostas JSON esperadas.
- [ ] **Step 2.5: Executar verificação**
  `cargo test --test mcp_protocol_test`.

---

### Task 3: Scanner Léxico Rápido (`ignore` + `grep-searcher`)

**Files:**
- Create: `src/search/mod.rs`
- Create: `src/search/scanner.rs`
- Test: `tests/scanner_test.rs`

- [ ] **Step 3.1: Escrever teste para o scanner léxico em `tests/scanner_test.rs`**
  Criar diretório temporário com arquivos `.rs`, `.kt`, `.txt`, `.gitignore` e um arquivo em `target/`. O teste deve verificar que termos buscados encontram linhas corretas e ignoram pastas excluídas.
- [ ] **Step 3.2: Implementar `LexicalScanner` em `src/search/scanner.rs`**
  Integrar `ignore::WalkBuilder` com tipos filtrados (`.rs`, `.kt`) e `grep_searcher::Searcher` com `grep_regex::RegexMatcher`.
- [ ] **Step 3.3: Implementar extração de matches**
  Retornar `RawMatch { file_path: PathBuf, line_number: usize, line_content: String }`.
- [ ] **Step 3.4: Executar verificação**
  `cargo test --test scanner_test`.

---

### Task 4: Chunker Sintático AST para Rust e Kotlin (`tree-sitter`)

**Files:**
- Create: `src/search/ast_chunker.rs`
- Modify: `src/search/mod.rs`
- Test: `tests/ast_chunker_test.rs`

- [ ] **Step 4.1: Escrever testes com fixtures em `tests/ast_chunker_test.rs`**
  - Fixture Rust: função com docstring e struct com métodos `impl`.
  - Fixture Kotlin: classe com anotações, `fun` e `companion object`.
  - Validar que dado o número de linha de um match dentro do corpo, o chunker sobe para a declaração raiz e extrai assinatura limpa e bloco delimitado.
- [ ] **Step 4.2: Implementar abstração de linguagem em `src/search/ast_chunker.rs`**
  Definir enum `SupportedLanguage { Rust, Kotlin }` mapeado por extensão (`.rs`, `.kt`).
- [ ] **Step 4.3: Implementar navegação de nós de escopo**
  - Mapear nós Rust: `function_item`, `impl_item`, `struct_item`, `enum_item`, `trait_item`.
  - Mapear nós Kotlin: `function_declaration`, `class_declaration`, `object_declaration`, `property_declaration`.
- [ ] **Step 4.4: Implementar extração de `AstSnippet` e deduplicação**
  Campos: `file_path`, `symbol_name`, `signature`, `code`, `start_line`, `end_line`. Limitar corpo a 80 linhas se for excessivamente longo.
- [ ] **Step 4.5: Executar verificação**
  `cargo test --test ast_chunker_test`.

---

### Task 5: Motor de Ranqueamento Léxico e Heurístico (Modo Fallback)

**Files:**
- Create: `src/search/ranker.rs`
- Modify: `src/search/mod.rs`
- Test: `tests/ranker_test.rs`

- [ ] **Step 5.1: Escrever teste de ranqueamento em `tests/ranker_test.rs`**
  Criar 3 snippets de código (um com o termo no nome da função, outro no corpo, outro apenas em comentário). O snippet com match no identificador deve ter pontuação superior.
- [ ] **Step 5.2: Implementar algoritmo heurístico de pontuação em `src/search/ranker.rs`**
  Calcular score baseado em:
  - Match exato no `symbol_name` (+3.0)
  - Match na `signature` (+2.0)
  - Frequência dos termos da query no `code` (+1.0 por ocorrência normalizada)
- [ ] **Step 5.3: Normalizar pontuação para intervalo $[0.0, 1.0]$ e ordenar top-$K$**
- [ ] **Step 5.4: Executar verificação**
  `cargo test --test ranker_test`.

---

### Task 6: Validador de Inferência Local `llama-cpp-2` com Logit Scoring e KV-Cache

**Files:**
- Create: `src/engine/mod.rs`
- Create: `src/engine/prompt.rs`
- Create: `src/engine/llama_validator.rs`
- Create: `src/engine/actor.rs`
- Test: `tests/engine_actor_test.rs`

- [ ] **Step 6.1: Implementar templates de prompt em `src/engine/prompt.rs`**
  Estruturar os templates com delimitadores estritos (`<|im_start|>system...`, `<|im_start|>user...`, `<|im_start|>assistant`) exigindo resposta binária S/N.
- [ ] **Step 6.2: Implementar `LlamaValidator` com Logit Scoring e KV-Cache Prefix Reuse usando `d3-edge`**
  Em `src/engine/llama_validator.rs`, configurar `LlamaBackend`, `LlamaModel` para carregar `vllm-d3-edge-Q8_0.gguf` e executar o prefill do prefixo uma única vez, avaliando candidatos pelo softmax dos tokens `S` e `N`.
- [ ] **Step 6.3: Implementar o Actor Pattern assíncrono em `src/engine/actor.rs`**
  Criar `ValidationActor` rodando em `std::thread::spawn` que atende canal `tokio::sync::mpsc::Receiver<ValidationRequest>` e responde via `tokio::sync::oneshot::Sender`.
- [ ] **Step 6.4: Implementar fallback automático caso modelo não esteja configurado**
  Se `model_path` for `None`, o ator delega imediatamente para o `LexicalRanker`.
- [ ] **Step 6.5: Escrever teste de ator em `tests/engine_actor_test.rs`**
  Testar envio assíncrono de snippets candidatos e recebimento de resultados no modo fallback.
- [ ] **Step 6.6: Executar verificação**
  `cargo test --test engine_actor_test`.

---

### Task 7: Schemas MCP e Implementação Completa da Tool `brain_search`

**Files:**
- Create: `src/mcp/schemas.rs`
- Create: `src/mcp/handlers.rs`
- Modify: `src/mcp/server.rs`
- Modify: `src/mcp/mod.rs`
- Test: `tests/brain_search_integration_test.rs`

- [ ] **Step 7.1: Definir schemas JSON das ferramentas em `src/mcp/schemas.rs`**
  Registrar schema do `brain_search` com `natural_query`, `keywords`, `max_results`, `min_confidence`.
- [ ] **Step 7.2: Implementar handler de `brain_search` em `src/mcp/handlers.rs`**
  Orquestrar: `scanner.scan()` $\rightarrow$ `ast_chunker.extract_snippets()` $\rightarrow$ `validator.validate()` $\rightarrow$ formatação Markdown cirúrgica.
- [ ] **Step 7.3: Conectar handler ao dispatcher em `src/mcp/server.rs`**
  Responder a `tools/call` com nome `brain_search`.
- [ ] **Step 7.4: Escrever teste de integração ponta a ponta em `tests/brain_search_integration_test.rs`**
  Criar mini-repositório com arquivos Rust e Kotlin, enviar comando JSON-RPC de `tools/call` e validar o retorno contendo as funções exatas e suas assinaturas.
- [ ] **Step 7.5: Executar verificação**
  `cargo test --test brain_search_integration_test`.

---

### Task 8: Motor de Grafo com `petgraph::StableDiGraph` e Watcher Incremental

**Files:**
- Create: `src/graph/mod.rs`
- Create: `src/graph/model.rs`
- Create: `src/graph/builder.rs`
- Create: `src/graph/queries.rs`
- Create: `src/graph/watcher.rs`
- Test: `tests/graph_test.rs`
- Test: `tests/watcher_test.rs`

- [ ] **Step 8.1: Implementar modelo de dados em `src/graph/model.rs`**
  Definir `SymbolNode`, `SymbolKind`, `RelationEdge`, `RelationKind` e `ProjectCodeGraph` com `StableDiGraph`.
- [ ] **Step 8.2: Implementar queries Tree-sitter para Rust e Kotlin em `src/graph/queries.rs`**
  Capturar definições de nós (funções, classes, structs) e arestas (calls, imports, uses_type).
- [ ] **Step 8.3: Implementar indexação em 2 passos em `src/graph/builder.rs`**
  Passo 1: Varredura de definições e criação de nós.
  Passo 2: Resolução de arestas de chamadas e imports.
- [ ] **Step 8.4: Implementar invalidação incremental em `src/graph/watcher.rs`**
  Integrar `notify` para escutar eventos de arquivo, calcular hash `blake3` e atualizar no `StableDiGraph` apenas os nós do arquivo modificado.
- [ ] **Step 8.5: Escrever testes em `tests/graph_test.rs` e `tests/watcher_test.rs`**
  Verificar resolução de chamadas entre dois arquivos e atualização incremental após modificação de arquivo.
- [ ] **Step 8.6: Executar verificação**
  `cargo test --test graph_test && cargo test --test watcher_test`.

---

### Task 9: Ferramentas de Grafo e Impacto: `brain_context_graph` e `brain_impact_check`

**Files:**
- Create: `src/graph/traversal.rs`
- Modify: `src/mcp/schemas.rs`
- Modify: `src/mcp/handlers.rs`
- Test: `tests/graph_tools_test.rs`

- [ ] **Step 9.1: Implementar extração de Ego-Graph em `src/graph/traversal.rs`**
  Percorrer $k$ saltos (`Incoming` e `Outgoing`) e gerar representação esqueleto em Markdown com assinaturas limpas.
- [ ] **Step 9.2: Implementar análise de impacto reverso em `src/graph/traversal.rs`**
  Percorrer todas as dependências transitivas que chamam ou usam o símbolo fornecido.
- [ ] **Step 9.3: Registrar schemas e handlers em `src/mcp/schemas.rs` e `src/mcp/handlers.rs`**
  Expor `brain_context_graph` e `brain_impact_check` na lista de ferramentas MCP.
- [ ] **Step 9.4: Escrever teste de integração em `tests/graph_tools_test.rs`**
  Validar a saída das ferramentas para símbolos específicos em Rust e Kotlin.
- [ ] **Step 9.5: Executar verificação**
  `cargo test --test graph_tools_test`.

---

### Task 10: Hardening do Daemon, Pauta Stdio e Suite de Verificação Final

**Files:**
- Modify: `src/main.rs`
- Create: `tests/daemon_smoke_test.rs`
- Create: `README.md`

- [ ] **Step 10.1: Garantir pureza total do stdio**
  Auditar todo o código para assegurar ausência de `println!` ou escritas não autorizadas no `stdout`.
- [ ] **Step 10.2: Escrever teste de smoke em `tests/daemon_smoke_test.rs`**
  Lançar o binário compilado como processo filho assíncrono (`tokio::process::Command`), enviar sequência real de MCP (`initialize` $\rightarrow$ `tools/list` $\rightarrow$ `brain_search`) via stdin e checar respostas válidas em stdout.
- [ ] **Step 10.3: Documentar instruções de instalação e configuração do servidor no `README.md`**
  Configurações de cliente MCP (Claude Code, Cursor) apontando para o binário.
- [ ] **Step 10.4: Executar checagem completa de qualidade**
  `cargo clippy --all-targets -- -D warnings` e `cargo test --all`.
