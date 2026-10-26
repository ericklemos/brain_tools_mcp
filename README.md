# 🧠 Brain-Tools (Rust MCP Server)

> **Servidor MCP contínuo (daemon stdio) em Rust para erradicar o *token maxxing* em agentes de codificação através de buscas cirúrgicas na AST e navegação em grafos contextuais.**

O **`brain-tools`** conecta o seu agente (Claude Code, Cursor, Zed, JetBrains, Cline) diretamente a um daemon ultrarrápido em Rust baseado no ecossistema Tokio. Em vez de ler arquivos inteiros ou depender de greps com linhas soltas, o `brain-tools` extrai apenas os blocos sintáticos semanticamente completos (funções, classes, structs, impls) e entrega grafos de chamadas compactos com assinaturas enxutas.

---

## 🚀 Principais Funcionalidades

1. **`brain_search` (Busca Cirúrgica AST-First):**
   - Varredura com `ignore` e `grep-searcher` respeitando `.gitignore` e ignorando `target/`, `build/`, `.gradle/`, `.git/`, `node_modules/`.
   - Expansão de nós sintáticos via **Tree-sitter** com suporte de primeira classe a **Rust** e **Kotlin**.
   - **Validação Local Inteligente:** Usa modelo GGUF **d3-edge** (`vllm-d3-edge-Q8_0.gguf`) com scoring binário de logits ($1$ forward pass) e reaproveitamento de KV-cache de prefixo na GPU via CUDA.
   - **Modo Fallback Automático:** Funciona perfeitamente mesmo sem modelo GGUF baixado, ranqueando blocos via densidade léxica/AST.

2. **`brain_context_graph` (Ego-Grafo de Chamadas):**
   - Grafo direcionado em memória (`petgraph::stable_graph::StableDiGraph`).
   - Retorna apenas as assinaturas de quem chama e do que o símbolo consome a $k$ saltos de distância, sem poluir o contexto com corpos inteiros.

3. **`brain_impact_check` (Análise de Impacto Reverso):**
   - Percorre dependências reversas transitivas no grafo para apontar todos os pontos do repositório que quebram caso uma assinatura seja modificada.

4. **Sincronização Incremental em Segundo Plano:**
   - Watcher assíncrono via `notify` e `blake3`.
   - Ao salvar um arquivo no editor, o grafo atualiza apenas os nós modificados em milissegundos sem reindexar o projeto todo.

---

## 📦 Download do Modelo GGUF (`d3-edge`)

O `brain-tools` é otimizado para o modelo de fundação para decisões **d3-edge** (0.7B da família vLLM Decision 3.0), pesando apenas ~806 MB e executando em milissegundos em GPUs como a RTX 5060:

```bash
# Executa o script automatizado de download (salva em models/vllm-d3-edge-Q8_0.gguf)
./scripts/download_model.sh
```

---

## 🛠️ Compilação e Execução

### Pré-requisitos
- Rust (edição 2021 ou superior)
- CMake e Clang
- (Opcional para GPU) Driver NVIDIA + CUDA Toolkit

### Build padrão (com fallback CPU/heurístico)
```bash
cargo build --release
```

### Build com aceleração CUDA (NVIDIA GPU)
```bash
cargo build --release --features cuda
```

O binário final estará em `target/release/brain-tools`.

### GPU versus fallback

O `--gpu-layers` só tem efeito quando o binário foi compilado com `--features cuda`
(ou `vulkan`). Um build padrão ignora a flag e roda em CPU — funciona, mas a validação
semântica fica bem mais lenta.

### Modelos não-causais (atenção: `vllm-d3-edge`)

O GGUF `d3-edge` declara `attention.causal = false`. Para modelos não-causais, o
llama.cpp exige `n_ubatch >= n_tokens` a cada `decode` (`llama-context.cpp:1789`);
quando o batch é menor, ele chama `GGML_ASSERT` e **aborta o processo com SIGABRT**
no meio da chamada MCP. O cliente só vê `Connection closed`, sem causa aparente.

Por isso o validador dimensiona `n_batch` e `n_ubatch` dinamicamente para o maior par
prefixo+candidato (teto de `n_ctx`, piso de 512). Ao trocar de modelo ou de tamanho de
snippet, os testes em `src/engine/llama_validator.rs` cobrem essa invariante:

```bash
cargo test --lib batch_capacity
```

---

## 📖 Documentação Detalhada de Conexão

Para um passo a passo completo sobre como adicionar o servidor via **Claude Code CLI (`claude mcp add`)**, **Cursor (`.cursor/mcp.json`)**, **Claude Desktop** e **Cline**, consulte o guia dedicado:
👉 [**Guia de Execução e Integração MCP**](file:///home/desktop/Documents/projects/brain_tools/docs/guia_de_execucao_e_integracao_mcp.md)

---

## ⚙️ Configuração no Cliente MCP

> **Transporte: stdio, sempre.** O `brain-tools` é um subprocesso que conversa por
> `stdin`/`stdout` — não existe endpoint HTTP. O cliente MCP o inicia e troca JSON-RPC
> pelo pipe. Se você procura por uma URL para configurar, essa opção não se aplica:
> o campo correto é `command` + `args`, nunca `url`.

### Opções de linha de comando

| Flag | Padrão | Descrição |
|---|---|---|
| `--root-dir`, `-r` | `.` (cwd) | Diretório indexado e vigiado pelo watcher. **Relativo ao cwd do processo.** |
| `--model-path` | — | Caminho do GGUF. Sem ele, opera em modo léxico/AST (fallback). |
| `--gpu-layers` | `33` | Camadas offloaded para a GPU. |
| `--min-confidence` | `0.65` | Confiança mínima para aprovar um bloco (0.0–1.0). |
| `--max-results` | `5` | Máximo de recortes retornados. |

Todas aceitam variável de ambiente equivalente (`BRAIN_ROOT_DIR`, `BRAIN_GPU_LAYERS`,
`BRAIN_MIN_CONFIDENCE`, `BRAIN_MAX_RESULTS`, `RUST_LOG`).

### Claude Code (`~/.claude.json`)

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/brain-tools/target/release/brain-tools",
      "args": [
        "--model-path", "/caminho/absoluto/brain-tools/models/vllm-d3-edge-Q8_0.gguf",
        "--gpu-layers", "33"
      ],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

**Por que omitir `--root-dir`:** o Claude Code inicia o servidor com cwd igual ao
diretório do projeto. Deixando o padrão `.`, o brain-tools indexa automaticamente o
repositório em que você está aberto — um único servidor configurado serve para todos os
projetos. Fixar `--root-dir` num caminho concreto faz ele indexar sempre o mesmo repo,
mesmo quando você trabalha em outro (a busca retorna "nenhum resultado" sem explicar
por quê).

> ⚠️ **Não use caminhos placeholder.** Um `--root-dir` inexistente não gera erro de
> configuração: o handshake JSON-RPC responde normalmente e o processo só morre depois,
> ao iniciar o watcher — o cliente reporta `Connection closed`.
> Confirme sempre com `test -d /seu/caminho`.

### Cursor (`.cursor/mcp.json`)

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/brain-tools/target/release/brain-tools",
      "args": ["--model-path", "/caminho/absoluto/brain-tools/models/vllm-d3-edge-Q8_0.gguf"]
    }
  }
}
```

### Preferir `brain_search` a `grep`

O MCP por si só não garante que o agente use a ferramenta — ele decide. Para orientar
essa escolha, adicione ao `CLAUDE.md` (global em `~/.claude/CLAUDE.md`, ou por projeto):

```markdown
## Busca de código

Prefira `brain_search` antes de `Grep` para buscas conceituais ("onde fica X").
Use `Grep`/`Glob` apenas para regex exata, strings literais ou quando já souber
o arquivo. Para avaliar impacto de mudanças, prefira `brain_context_graph` e
`brain_impact_check` a varrer o repo caçando chamadas.
```

Se uma chamada MCP falhar, o agente deve dizer isso explicitamente em vez de
silenciosamente seguir com grep — a degradação da busca merece ser sinalizada.

---

## 🧪 Suíte de Testes

Para rodar todos os testes de unidade, integração e smoke tests:
```bash
cargo test
```
