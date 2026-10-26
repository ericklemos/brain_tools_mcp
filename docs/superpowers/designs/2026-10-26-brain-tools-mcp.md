# Design: Brain-Tools MCP Server (Daemon de Busca Cirúrgica e Contexto em Rust)

- **Data:** 2026-10-26
- **Status:** Proposto
- **Autores:** Pair Antigravity & Usuário
- **Referência:** `arquitetura_e_especifica_o_do_brain_tools.md`

---

## 1. Contexto e Motivação (Intent & Why)

Agentes de desenvolvimento de software frequentemente sofrem de *token maxxing* e degradação de atenção ao tentar compreender código em repositórios médios e grandes. As ferramentas padrão de busca (grep, ripgrep, leitura de arquivos na íntegra) retornam ou linhas órfãs sem escopo léxico/sintático ou despejam arquivos inteiros de centenas de linhas no contexto do LLM.

O **`brain-tools`** nasce como um servidor local **MCP (Model Context Protocol)** contínuo e persistente, construído em **Rust**, projetado para rodar ininterruptamente como um daemon em segundo plano acoplado aos clientes MCP (Claude Code, Cursor, JetBrains, etc.). Ele elimina o excesso de contexto combinando:
1. Varredura rápida de texto (`ignore` + `grep-searcher`).
2. Expansão para nós sintáticos completos via **Tree-sitter** (com foco primário em **Rust e Kotlin**).
3. Ranqueamento com validação local de baixa latência via **llama-cpp-2** (com suporte a GPU CUDA, scoring binário e reutilização de KV-cache) ou fallback inteligente baseado em densidade léxica/AST.
4. Grafo de chamadas e impacto em memória (`petgraph::StableDiGraph`) atualizado incrementalmente via `notify` + `blake3`.

---

## 2. O que Significa Sucesso (What Success Looks Like)

- **Sempre Ativo (Always-On MCP):** O processo inicializa como um servidor stdio JSON-RPC 2.0 resiliente. Ele mantém modelos em VRAM, grafos e árvores em memória sem vazamento de recursos ou travamentos por dias de uso contínuo.
- **Recorte Cirúrgico:** A ferramenta `brain_search` nunca devolve linhas soltas ou arquivos gigantescos; ela devolve assinaturas e blocos completos de funções/classes/structs com localização precisa (`file:start_line-end_line`).
- **Autonomia Out-of-the-Box (Fallback):** Se o modelo GGUF não estiver especificado ou disponível, o servidor continua 100% funcional, ranqueando blocos recortados pela pontuação léxica AST. Quando configurado com GGUF, a GPU (CUDA) acelera o filtro semântico com latência de ~15-35ms por candidato.
- **Suporte Prioritário:** Rust (`.rs`) e Kotlin (`.kt`) mapeados com precisão sintática em todos os extratores e no grafo.

---

## 3. Escopo (Scope)

### No Escopo (In-Scope - Fase 1 e 2)
- **Servidor MCP stdio:** Implementação pura do protocolo MCP sobre `stdin`/`stdout` (`initialize`, `tools/list`, `tools/call`, `notifications/initialized`).
- **Gramáticas de Primeira Classe:** Suporte completo via `tree-sitter-rust` e `tree-sitter-kotlin`.
- **Pipeline `brain_search`:**
  - Varredura com `ignore` (respeito a `.gitignore`, exclusão de `target`, `build`, `.gradle`, `.git`).
  - `ast_chunker` com localização de escopos semânticos (funções, classes, structs, impls, traits, interfaces).
  - Deduplicação de nós e truncamento inteligente de corpo de função.
  - Modo Fallback (Lexical/AST ranking) nativo.
  - Modo GGUF via `llama-cpp-2` (Actor pattern em thread dedicada com canal assíncrono).
- **Watcher Incremental:** Invalidação de arquivos modificados em disco via `notify` e comparação de hash `blake3`.

### No Escopo (Fase 3 - Grafo e Impacto)
- **`brain_context_graph`:** Consulta de ego-grafo a $k$ saltos (incoming/outgoing calls e uses_type) com renderização de esqueletos (apenas assinaturas).
- **`brain_impact_check`:** Análise de chamadas reversas para prever impacto de mudanças em símbolos.

### Fora de Escopo Inicial (Out-of-Scope)
- Interface de linha de comando ad-hoc (CLI commands como `brain-tools search`) — o binário é estritamente um servidor MCP.
- Linguagens secundárias (Python, TypeScript, Go) postergadas para iterações seguintes após estabilização de Rust e Kotlin.
- Decodificação autorregressiva livre de texto pelo modelo local (apenas logit scoring binário de $1$ token).

---

## 4. Arquitetura e Fluxo de Dados

```
                      [ Cliente MCP: Claude Code / Cursor ]
                                      │
                         JSON-RPC 2.0 │ (stdio: stdin / stdout)
                                      ▼
                      ┌────────────────────────────────┐
                      │    mcp::protocol / handlers    │
                      └───────────────┬────────────────┘
                                      │
            ┌─────────────────────────┼─────────────────────────┐
            │                         │                         │
            ▼                         ▼                         ▼
   [ brain_search ]       [ brain_context_graph ]     [ brain_impact_check ]
            │                         │                         │
  ┌─────────┴─────────┐               └────────────┬────────────┘
  │ 1. scanner        │                            ▼
  │    (ignore + grep)│               ┌───────────────────────────┐
  │ 2. ast_chunker    │               │ graph::CodeGraph          │
  │    (Rust/Kotlin)  │               │ (petgraph::StableDiGraph) │
  └─────────┬─────────┘               └────────────▲──────────────┘
            │                                      │
    Possui GGUF?                                   │
      ├─ Não ──> [ AST Lexical Ranker ]            │ watcher (notify + blake3)
      │                                            │
      └─ Sim ──> [ LlamaValidator Actor (CUDA) ]   │
                 (1 Forward Pass Logits S/N)       │
```

---

## 5. Decisões Técnicas Principais

1. **Adoção Integral do Ecossistema Tokio & Crates Padrão da Indústria:**
   - **Runtime & I/O:** `tokio` (com `features = ["full"]`) e `tokio-util` (para codecs de linha/frames em streams assíncronos de `stdin`/`stdout`).
   - **Diagnóstico e Observabilidade:** `tracing` + `tracing-subscriber` (com filtro `EnvFilter`), direcionando logs estritamente para `stderr` (`std::io::stderr`) para manter o `stdout` 100% puro para o JSON-RPC do MCP.
   - **Configuração:** `clap` (v4 com `features = ["derive", "env"]`) para parsing limpo e padronizado de flags CLI e variáveis de ambiente.
   - **Erros Idiomáticos:** `thiserror` para tipos de erros de domínio bem definidos e `anyhow` para fronteiras de inicialização/aplicação.
   - **Concorrência e Estado Compartilhado:** `dashmap` e canais `tokio::sync::{mpsc, oneshot, watch}` para comunicação segura entre a task MCP e os motores.
   - **Varredura e AST:** Crates oficiais do ecossistema ripgrep (`ignore`, `grep-searcher`, `grep-regex`) e `tree-sitter` (`tree-sitter-rust`, `tree-sitter-kotlin`).

2. **Protocolo MCP Estrito via Stdio:** Sem poluição de stdout. Todas as mensagens de log internas são enviadas exclusivamente via `stderr` usando `tracing_subscriber` com filtro configurável (`RUST_LOG=info`).
3. **Ciclo de Vida Persistente (Daemon):** O servidor inicia uma vez e mantém conexões abertas. Não há re-parse global a cada busca:
   - Os hashes de arquivos e nós no grafo são atualizados sob demanda pelo `notify`.
   - O contexto do modelo `llama-cpp-2` permanece aquecido na VRAM.
4. **Modelo Padrão e Estratégia de Fallback sem GGUF:**
   - **Modelo Designado:** `d3-edge` (especificamente `vllm-d3-edge-Q8_0.gguf` da organização `ggml-org/vllm-d3-edge-GGUF`). Trata-se de um modelo de fundação para decisões de 0.7B parâmetros da família vLLM Decision 3.0, extremamente veloz em GPU (806 MB, forward pass sub-15ms em RTX 5060).
   - **Script de Download:** `scripts/download_model.sh` baixa o modelo automaticamente para o diretório `models/`.
   - **Modo Fallback:** O campo `model_path` na configuração é opcional (`Option<PathBuf>`), apontando por padrão para `models/vllm-d3-edge-Q8_0.gguf` se existente. Se ausente ou inexistente, o validador entra automaticamente em `ScoringMode::LexicalHeuristic`, atribuindo pontuações baseadas na contagem de matches e relevância estrutural (match no nome da função tem peso maior que match em comentário).
5. **Tratamento de Gramática Kotlin:**
   - Crate: `tree-sitter-kotlin`.
   - Nós de busca: `function_declaration`, `class_declaration`, `object_declaration`, `property_declaration`, `companion_object`.
   - Mapeamento de escopo: Identificação da raiz da declaração navegando `node.parent()` até o nível de declaração de membro ou classe.

---

## 6. Modos de Falha e Resiliência

| Modo de Falha | Prevenção e Tratamento |
|---|---|
| Arquivo sintaticamente quebrado ou com erro de compilação durante edição | `tree-sitter` possui parser resiliente com nós `ERROR`; o chunker ignora nós inválidos e extrai o bloco ao redor sem entrar em pânico. |
| OOM de GPU ao alocar contexto GGUF grande | Parâmetros de contexto configuráveis (`--n-ctx`, padrão 4096), verificação de memória disponível e fallback suave para CPU ou modo heurístico se falhar a inicialização CUDA. |
| Arquivos gigantescos minificados ou binários | Filtro via `ignore` e teto máximo de tamanho de arquivo (ex.: 1MB) antes da varredura léxica. |
| Travamento no canal do ator Llama | Timeout explícito nas chamadas `oneshot::channel` com resposta de fallback caso a inferência exceda 500ms. |

---

## 7. Próximos Passos

Passar para o **Plano de Implementação Detalhado** (`writing-plans`), decompondo o projeto em fases de execução testáveis:
- **Fase 1:** Scaffolding do projeto Rust, tipos de configuração, protocolo MCP stdio base e canal de log stderr.
- **Fase 2:** Scanner léxico (`ignore` + `grep-searcher`) e extrator de AST para Rust e Kotlin (`ast_chunker`).
- **Fase 3:** Implementação do validador (modo Heurístico Fallback + modo GGUF `llama-cpp-2` com CUDA).
- **Fase 4:** Integração e entrega da tool `brain_search` via MCP stdio.
- **Fase 5:** Motor de grafo (`petgraph`), watcher incremental e tools `brain_context_graph` + `brain_impact_check`.
