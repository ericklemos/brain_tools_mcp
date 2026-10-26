# Brain-Tools: Especificação Técnica e Blueprint de Implementação (Rust MCP)

O **brain-tools** é um servidor MCP (*Model Context Protocol*) local construído em Rust cujo objetivo é erradicar o *token maxxing* em agentes de codificação. Em vez de despejar arquivos inteiros na janela de contexto do LLM principal, o `brain-tools` combina varredura léxica, análise sintática via AST (`tree-sitter`), grafos direcionados (`petgraph`) e um motor de inferência local embarcado (`llama-cpp-2`) para entregar apenas recortes cirúrgicos e grafos de contexto validados.

---

## 1. Estrutura do Projeto e `Cargo.toml`

### Árvore de Diretórios
```text
brain-tools/
├── Cargo.toml
├── src/
│   ├── main.rs                 # Bootstrap do servidor MCP (stdio JSON-RPC) e inicialização de estado
│   ├── config.rs               # Parâmetros de CLI/Env (caminho do GGUF, threads, GPU layers, root_dir)
│   ├── mcp/
│   │   ├── mod.rs
│   │   ├── protocol.rs         # Tipos JSON-RPC 2.0 e despacho de tools
│   │   ├── schemas.rs          # Schemas JSON das ferramentas (brain_search, brain_graph, brain_impact)
│   │   └── handlers.rs         # Orquestração entre Motor Léxico/AST, Petgraph e LlamaEngine
│   ├── engine/
│   │   ├── mod.rs
│   │   ├── llama_validator.rs  # Wrapper do llama-cpp-2 com KV-Cache Prefix Reuse e Logit Scoring
│   │   └── prompt.rs           # Templates de prompt para validação binária (S/N)
│   ├── search/
│   │   ├── mod.rs
│   │   ├── scanner.rs          # Varredura rápida com `ignore` + `grep-searcher`
│   │   └── ast_chunker.rs      # Extração de blocos sintáticos completos via `tree-sitter`
│   └── graph/
│       ├── mod.rs
│       ├── model.rs            # Definições de SymbolNode, RelationEdge e CodeGraph (StableDiGraph)
│       ├── builder.rs          # Indexação em 2 passos (Coleta de Símbolos -> Resolução de Arestas)
│       ├── queries.rs          # Queries S-expression do tree-sitter (Rust, TS/JS, Python, Go)
│       ├── traversal.rs        # Extração de Ego-Graph (k-hops) e Skeleton Rendering
│       └── watcher.rs          # Invalidação incremental com `notify` e `blake3`
```

### `Cargo.toml`
```toml
[package]
name = "brain-tools"
version = "0.1.0"
edition = "2021"

[dependencies]
# Runtime Assíncrono e Serialização (MCP Protocol)
tokio = { version = "1.40", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
anyhow = "1.0"
thiserror = "1.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Motor de Inferência Local (GGUF Nativo)
llama-cpp-2 = { version = "0.1", features = ["cuda"] } # Ajustar feature conforme hardware: "metal", "vulkan", "cuda"
crossbeam-channel = "0.5"

# Varredura de Arquivos e Busca Léxica
ignore = "0.4"
grep-searcher = "0.1"
grep-regex = "0.1"
grep-matcher = "0.1"

# Parser AST e Gramáticas
tree-sitter = "0.24"
tree-sitter-rust = "0.23"
tree-sitter-typescript = "0.23"
tree-sitter-python = "0.23"
streaming-iterator = "0.1"

# Motor de Grafo e Indexação Incremental
petgraph = { version = "0.6", features = ["serde-1"] }
blake3 = "1.5"
notify = "6.1"
dashmap = "6.1"
```

---

## 2. Motor Local com `llama-cpp-2`: Baixa Latência e Zero-Generation

Na ecossistema Rust atual, a crate recomendada é a **`llama-cpp-2`** (mantida pelo grupo `utilityai` com suporte atualizado aos modelos GGUF mais recentes).

Para que a validação local de múltiplos candidatos de código não trave a resposta do servidor MCP, **não fazemos geração livre de texto** (*autoregressive decoding*). Aplicamos três otimizações fundamentais:

### 2.1. Dedicação de Thread Síncrona (Actor Pattern)
O `LlamaContext` não é `Sync`. Portanto, o motor roda em uma `std::thread::spawn` dedicada (ou `tokio::task::spawn_blocking` persistente) que recebe requisições via canal (`tokio::sync::mpsc` + `oneshot`) do loop assíncrono do MCP.

### 2.2. Logit Scoring de Token Único ($1\text{ Forward Pass}$)
Em vez de gerar uma explicação, estruturamos o prompt para exigir uma resposta de um único caractere (`S` para relevante, `N` para irrelevante). Fazemos apenas o *prefill* (`ctx.decode(&mut batch)`) e inspecionamos diretamente o vetor de logits do último token para calcular a probabilidade real via Softmax restrito aos dois tokens:

$$P(\text{Relevante}) = \frac{e^{\text{logit}(S)}}{e^{\text{logit}(S)} + e^{\text{logit}(N)}}$$

Se $P(\text{Relevante}) \ge \text{threshold}$ (ex.: $0.65$), o bloco de código é aprovado. Isso reduz o tempo de avaliação de ~600ms para **15ms a 35ms por função**.

### 2.3. KV-Cache Prefix Reuse
Quando uma busca retorna $N$ candidatos para a mesma pergunta do usuário, o System Prompt e a Query do usuário permanecem idênticos.
1. Avaliamos a sequência `[System Prompt + User Query]` uma única vez no `LlamaBatch` e guardamos a posição `n_past_prefix`.
2. Para cada candidato $i$, chamamos `ctx.clear_kv_cache_seq(Some(0), Some(n_past_prefix), None)` para limpar apenas o sufixo anterior, inserimos o código do candidato $i$ a partir de `n_past_prefix` e executamos um único `decode`.

### Esqueleto de Implementação (`src/engine/llama_validator.rs`)
```rust
use anyhow::{Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel, Special};
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::Path;

pub struct CandidateSnippet {
    pub id: usize,
    pub file_path: String,
    pub symbol_name: String,
    pub code: String,
}

pub struct ScoredSnippet {
    pub id: usize,
    pub probability: f32,
}

pub struct LlamaValidator {
    backend: LlamaBackend,
    model: LlamaModel,
    token_yes: LlamaToken,
    token_no: LlamaToken,
}

impl LlamaValidator {
    pub fn new(model_path: &Path, gpu_layers: u32) -> Result<Self> {
        let backend = LlamaBackend::init()?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
        let model = LlamaModel::load_from_file(&backend, model_path, &model_params)
            .context("Falha ao carregar arquivo GGUF")?;

        // Resolve os IDs dos tokens "S" e "N" na inicialização
        let token_yes = model.str_to_token("S", AddBos::Never)?[0];
        let token_no = model.str_to_token("N", AddBos::Never)?[0];

        Ok(Self {
            backend,
            model,
            token_yes,
            token_no,
        })
    }

    pub fn validate_batch(
        &self,
        user_query: &str,
        candidates: &[CandidateSnippet],
        threshold: f32,
    ) -> Result<Vec<ScoredSnippet>> {
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(4096))
            .with_n_batch(2048);
        let mut ctx = self.model.new_context(&self.backend, ctx_params)?;

        // 1. Prefixo fixo (System + Query)
        let prefix_str = format!(
            "<|im_start|>system\nVocê é um filtro estrito de código. Avalie se o trecho de código responde ou implementa o que o usuário busca. Responda APENAS 'S' ou 'N'.<|im_end|>\n<|im_start|>user\nBusca: {}\n",
            user_query
        );
        let prefix_tokens = self.model.str_to_token(&prefix_str, AddBos::Always)?;
        let n_past_prefix = prefix_tokens.len() as i32;

        let mut batch = LlamaBatch::new(2048, 1);
        for (i, &token) in prefix_tokens.iter().enumerate() {
            batch.add(token, i as i32, &[0], false)?;
        }
        ctx.decode(&mut batch)?;

        let mut approved = Vec::new();

        // 2. Avalia cada candidato reaproveitando o KV-Cache do prefixo
        for candidate in candidates {
            ctx.clear_kv_cache_seq(Some(0), Some(n_past_prefix as u32), None)?;
            batch.clear();

            let suffix_str = format!(
                "Arquivo: {}\nSímbolo: {}\nCódigo:\n```\n{}\n```\nRelevante (S/N)?<|im_end|>\n<|im_start|>assistant\n",
                candidate.file_path, candidate.symbol_name, candidate.code
            );
            let suffix_tokens = self.model.str_to_token(&suffix_str, AddBos::Never)?;

            for (j, &token) in suffix_tokens.iter().enumerate() {
                let pos = n_past_prefix + (j as i32);
                let is_last = j == suffix_tokens.len() - 1;
                batch.add(token, pos, &[0], is_last)?;
            }

            ctx.decode(&mut batch)?;

            // Extrai logits do último token processado
            let logits = ctx.candidates_ith(batch.n_tokens() - 1);
            let mut logit_s = f32::NEG_INFINITY;
            let mut logit_n = f32::NEG_INFINITY;

            for token_data in logits {
                if token_data.id() == self.token_yes {
                    logit_s = token_data.logit();
                } else if token_data.id() == self.token_no {
                    logit_n = token_data.logit();
                }
            }

            // Softmax binário entre S e N
            let max_logit = logit_s.max(logit_n);
            let exp_s = (logit_s - max_logit).exp();
            let exp_n = (logit_n - max_logit).exp();
            let prob_yes = exp_s / (exp_s + exp_n);

            if prob_yes >= threshold {
                approved.push(ScoredSnippet {
                    id: candidate.id,
                    probability: prob_yes,
                });
            }
        }

        approved.sort_by(|a, b| b.probability.partial_cmp(&a.probability).unwrap());
        Ok(approved)
    }
}
```

---

## 3. Ferramenta 1: Pipeline de Pesquisa em Arquivos (`AST-First`)

A ferramenta `brain_search` evita enviar arquivos inteiros ou linhas órfãs sem contexto. O fluxo opera em 3 fases:

### Fase 1: Filtragem Léxica Paralela (`src/search/scanner.rs`)
1. Extrai termos-chave da query natural (ou recebe identificadores/regex opcionais).
2. Usa `ignore::WalkBuilder` para varrer o repositório respeitando `.gitignore` e ignorando diretórios pesados (`target/`, `node_modules/`, `.git/`).
3. Usa `grep_searcher::Searcher` para localizar rapidamente o *byte offset* e o número da linha onde os termos ocorrem.

### Fase 2: Expansão para Bloco Sintático (`src/search/ast_chunker.rs`)
1. Para cada arquivo com *match*, faz o parse rápido com `tree-sitter`.
2. Para cada linha encontrada no grep, desce até o `descendant_for_point_range` correspondente e sobe na árvore (`node.parent()`) até atingir um nó de escopo semântico completo:
   * **Rust:** `function_item`, `impl_item`, `struct_item`, `enum_item`, `trait_item`
   * **TypeScript/JS:** `function_declaration`, `method_definition`, `class_declaration`, `interface_declaration`
   * **Python:** `function_definition`, `class_definition`
3. Deduplica os blocos caso múltiplos matches caiam dentro da mesma função.
4. Extrai também o cabeçalho daquele nó (assinatura + docstrings) e limita o corpo a um teto seguro (ex.: 80 linhas) antes de enviar ao `LlamaValidator`.

### Fase 3: Validação Local e Resposta Cirúrgica
1. Envia os blocos candidatos ao `LlamaValidator::validate_batch`.
2. Descarta tudo abaixo do `threshold` e retorna apenas o Top-$K$ blocos validados com metadados exatos (`file_path`, `start_line`, `end_line`, `symbol_name`, `confidence`, `code_block`).

---

## 4. Ferramenta 2: Grafo de Chamada e Contexto com `petgraph`

### 4.1. Modelagem Estrutural (`src/graph/model.rs`)
Usamos **`petgraph::stable_graph::StableDiGraph`** em vez de `DiGraph` padrão. No `DiGraph`, remover um nó invalida os `NodeIndex` existentes (fazendo swap com o último elemento). Com o `StableDiGraph`, quando um arquivo é modificado, podemos remover apenas os nós daquele arquivo e reindexá-lo em milissegundos mantendo todos os `NodeIndex` do projeto intactos.

```rust
use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolKind {
    FileModule,
    Function,
    Method,
    StructOrClass,
    TraitOrInterface,
    Enum,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub qualified_name: String, // ex: "crate::auth::jwt::verify_token"
    pub name: String,           // ex: "verify_token"
    pub kind: SymbolKind,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,      // APENAS a assinatura: "pub fn verify_token(tok: &str) -> Result<Claims>"
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelationKind {
    Defines,         // Arquivo X define Função/Struct A
    Imports,         // Arquivo X importa Módulo/Símbolo de Y
    Calls,           // Função A (em X) invoca Função/Método B (em Y)
    UsesType,        // Função/Struct A recebe ou retorna Tipo T (definido em Y)
    ImplementsTrait, // Struct A implementa Trait/Interface I
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationEdge {
    pub kind: RelationKind,
    pub call_site_line: usize, // Linha exata onde a chamada ou import ocorre
}

pub struct ProjectCodeGraph {
    pub graph: StableDiGraph<SymbolNode, RelationEdge>,
    /// Índice rápido por nome simples -> lista de NodeIndex (para resolver chamadas)
    pub symbol_lookup: HashMap<String, Vec<NodeIndex>>,
    /// Índice por caminho qualificado -> NodeIndex único
    pub qualified_lookup: HashMap<String, NodeIndex>,
    /// Nós pertencentes a cada arquivo (para invalidação incremental rápida)
    pub file_nodes: HashMap<PathBuf, Vec<NodeIndex>>,
    /// Hash blake3 do conteúdo de cada arquivo
    pub file_hashes: HashMap<PathBuf, blake3::Hash>,
}
```

### 4.2. Algoritmo de Construção em 2 Passos (`src/graph/builder.rs`)
Como um arquivo `X` pode chamar uma função definida em um arquivo `Y` que ainda não foi lido, a construção do grafo acontece em duas passadas rápidas com `tree-sitter`:

* **Passo 1 — Coleta de Definições e Assinaturas (Criação de Nós):**
  * Varre todos os arquivos do projeto, calcula o hash `blake3` e executa a query de definições do `tree-sitter`.
  * Extrai a **assinatura limpa** (sem o bloco `{ ... }` do corpo da função) e insere o `SymbolNode` no `StableDiGraph`, populando `symbol_lookup` e `file_nodes`.
* **Passo 2 — Resolução de Imports, Chamadas e Tipos (Criação de Arestas):**
  * Executa a query de referências do `tree-sitter` sobre cada função/arquivo:
    * Captura `use_declaration` / `import_statement` $\rightarrow$ cria aresta `RelationKind::Imports`.
    * Captura `call_expression` (ex.: `token_service.validate()`) $\rightarrow$ cruza o identificador chamado com os imports do arquivo atual e com o `symbol_lookup` para ligar o nó da função chamadora ao nó da função chamada via `RelationKind::Calls`.
    * Captura `type_identifier` nos parâmetros e retorno da assinatura $\rightarrow$ cria aresta `RelationKind::UsesType`.

### 4.3. Queries S-Expression do `tree-sitter` (Exemplo Rust)
```scheme
;; Query 1: Definições (Extração de Nós)
(function_item
  name: (identifier) @fn.name
  parameters: (parameters) @fn.params
  return_type: (_)? @fn.return) @fn.def

(struct_item
  name: (type_identifier) @struct.name) @struct.def

(trait_item
  name: (type_identifier) @trait.name) @trait.def

;; Query 2: Conexões e Chamadas (Extração de Arestas)
(use_declaration
  argument: (_) @import.path)

(call_expression
  function: [
    (identifier) @call.direct
    (field_expression field: (field_identifier) @call.method)
    (scoped_identifier name: (identifier) @call.scoped)
  ])
```

### 4.4. Extração de Ego-Graph e Skeleton Rendering (`src/graph/traversal.rs`)
Quando o LLM externo invoca o grafo de contexto para um símbolo ou arquivo, o `brain-tools` **nunca** envia o código inteiro dos arquivos vizinhos. Ele executa uma busca em largura limitada a $k$ saltos ($k = 1$ ou $2$) usando `petgraph::Direction::Incoming` (quem me chama) e `petgraph::Direction::Outgoing` (quem eu chamo e quais tipos eu uso):

* **Nó Central (Distância $0$):** Retorna localização exata, assinatura e conexões diretas.
* **Vizinhos de $1^\circ$ e $2^\circ$ Grau (Distância $1..k$):** Retorna **apenas a `signature`** armazenada no `SymbolNode`, o caminho do arquivo e o tipo da aresta (`RelationKind` + linha do call-site).

---

## 5. Contratos JSON-RPC das Ferramentas MCP (`src/mcp/schemas.rs`)

O servidor expõe três ferramentas cirúrgicas para o cliente MCP (Claude Code, Cursor, Cline, etc.):

### Tool 1: `brain_search`
Pesquisa código no projeto usando filtragem léxica + recorte de AST + validação semântica via modelo GGUF local.
* **Input Schema:**
```json
{
  "type": "object",
  "properties": {
    "natural_query": {
      "type": "string",
      "description": "Descrição em linguagem natural do comportamento ou regra de negócio procurada."
    },
    "keywords": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Termos técnicos, nomes de funções ou regex curtas para ancorar a busca rápida no código."
    },
    "max_results": {
      "type": "integer",
      "default": 5
    },
    "min_confidence": {
      "type": "number",
      "default": 0.65
    }
  },
  "required": ["natural_query", "keywords"]
}
```

### Tool 2: `brain_context_graph`
Mapeia como um símbolo ou arquivo `X` se conecta a outros arquivos `Y` (chamadas, imports, tipos usados), retornando apenas assinaturas enxutas.
* **Input Schema:**
```json
{
  "type": "object",
  "properties": {
    "target_symbol_or_file": {
      "type": "string",
      "description": "Nome da função/struct (ex: 'verify_token') ou caminho relativo do arquivo."
    },
    "depth": {
      "type": "integer",
      "default": 1,
      "minimum": 1,
      "maximum": 3,
      "description": "Número de saltos no grafo de chamadas e dependências."
    }
  },
  "required": ["target_symbol_or_file"]
}
```
* **Exemplo de Output Compacto (Markdown Otimizado para LLM):**
```markdown
### Alvo: `fn verify_token` (`src/auth/jwt.rs:42-78`)
Assinatura: `pub fn verify_token(raw: &str, secret: &SecretKey) -> Result<Claims, AuthError>`

#### Quem chama este símbolo (Incoming Calls):
- `src/http/middleware.rs:29` -> `fn auth_guard(req: Request)` [Calls na linha 34]
- `src/ws/handshake.rs:51` -> `fn on_connect( peer: Socket)` [Calls na linha 60]

#### O que este símbolo consome (Outgoing Calls & Types):
- `src/auth/keys.rs:12` -> `pub struct SecretKey { ... }` [UsesType]
- `src/auth/claims.rs:8` -> `pub struct Claims { pub sub: String, pub exp: u64 }` [UsesType]
- `src/crypto/hmac.rs:19` -> `pub fn verify_signature(payload: &[u8], sig: &[u8]) -> bool` [Calls na linha 55]
```

### Tool 3: `brain_impact_check`
Percorre as arestas reversas (`Incoming`) no `StableDiGraph` para listar todos os pontos do sistema que serão quebrados ou impactados caso uma assinatura ou struct seja alterada.
* **Input Schema:**
```json
{
  "type": "object",
  "properties": {
    "symbol_name": {
      "type": "string",
      "description": "Nome da função, método ou struct que será modificada."
    }
  },
  "required": ["symbol_name"]
}
```

---

## 6. Roteiro de Implementação (Passo a Passo)

1. **Etapa 1 — Core do Grafo e AST (`src/graph/` + `src/search/`):**
   * Implementar o parser `tree-sitter` extraindo `SymbolNode` (com assinatura limpa) e populando o `StableDiGraph`.
   * Implementar o extrator de blocos sintáticos completos a partir de linhas retornadas pelo `grep-searcher`.
2. **Etapa 2 — Validador Embarcado (`src/engine/llama_validator.rs`):**
   * Testar o carregamento de um modelo leve (ex.: `Qwen2.5-Coder-1.5B-Instruct-Q4_K_M.gguf` ou `3B`) e validar a latência do *Logit Scoring* com *KV-Cache Prefix Reuse*.
3. **Etapa 3 — Loop JSON-RPC do Servidor MCP (`src/mcp/` + `src/main.rs`):**
   * Expor as três ferramentas via `stdio` seguindo a especificação MCP (`initialize`, `tools/list`, `tools/call`) e ativar o watcher incremental (`notify` + `blake3`).