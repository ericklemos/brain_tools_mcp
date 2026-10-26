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

---

## 📖 Documentação Detalhada de Conexão

Para um passo a passo completo sobre como adicionar o servidor via **Claude Code CLI (`claude mcp add`)**, **Cursor (`.cursor/mcp.json`)**, **Claude Desktop** e **Cline**, consulte o guia dedicado:
👉 [**Guia de Execução e Integração MCP**](file:///home/desktop/Documents/projects/brain_tools/docs/guia_de_execucao_e_integracao_mcp.md)

---

## ⚙️ Configuração no Cliente MCP

### Claude Code (`~/.claude.json` ou flag `--mcp`)
```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/brain-tools/target/release/brain-tools",
      "args": [
        "--root-dir", "/caminho/do/seu/projeto",
        "--model-path", "/caminho/absoluto/brain-tools/models/vllm-d3-edge-Q8_0.gguf"
      ],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

### Cursor (`.cursor/mcp.json`)
```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/brain-tools/target/release/brain-tools",
      "args": ["--root-dir", "."]
    }
  }
}
```

---

## 🧪 Suíte de Testes

Para rodar todos os testes de unidade, integração e smoke tests:
```bash
cargo test
```
