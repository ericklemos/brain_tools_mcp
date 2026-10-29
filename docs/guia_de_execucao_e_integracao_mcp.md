# Guia Completo de Execução e Conexão ao Brain-Tools MCP

Este guia descreve passo a passo como compilar, executar e conectar o servidor MCP **`brain-tools`** aos seus clientes de IA, com foco em arquivos de configuração e comandos via CLI do **Claude (Claude Code)** e **Cursor**.

---

## 1. Preparação e Inicialização do Servidor

### Passo 1: Compilação do Executável

```bash
cargo build --release
```

O binário final compilado estará localizado em:
```text
/home/desktop/Documents/projects/brain_tools/target/release/brain-tools
```

### Passo 2: Teste de Inicialização Direta no Terminal
Você pode testar a inicialização do daemon diretamente no terminal. Como o protocolo MCP opera em modo `stdio`, o servidor aguardará mensagens JSON-RPC no `stdin` e emitirá seus logs formatados no `stderr`:

```bash
# Executa apontando para o diretório de um repositório
./target/release/brain-tools --root-dir .
```

*Saída esperada no `stderr`:*
```text
INFO Starting brain-tools MCP server daemon version="0.1.0" root_dir="."
```
*(Pressione `Ctrl + C` para encerrar o teste).*

---

## 2. Conectando via CLI do Claude (Claude Code)

O **Claude Code** oferece comandos diretos no terminal para gerenciar servidores MCP sem que você precise editar arquivos JSON manualmente.

### Adicionar o `brain-tools` via `claude mcp add`

#### Modo com diretório explícito:
```bash
claude mcp add brain-tools \
  /home/desktop/Documents/projects/brain_tools/target/release/brain-tools \
  --root-dir /caminho/do/seu/projeto
```

#### Modo Dinâmico (adotando o diretório atual do projeto em execução):
Se você preferir que o `brain-tools` utilize o diretório onde o Claude for iniciado:
```bash
claude mcp add brain-tools \
  /home/desktop/Documents/projects/brain_tools/target/release/brain-tools \
  --root-dir .
```

### Comandos Úteis do Claude CLI

- **Listar ferramentas e servidores ativos:**
  ```bash
  claude mcp list
  ```
  *(Você verá `brain-tools` com as ferramentas `brain_search`, `brain_context_graph` e `brain_impact_check`).*

- **Consultar detalhes do servidor:**
  ```bash
  claude mcp get brain-tools
  ```

- **Remover o servidor:**
  ```bash
  claude mcp remove brain-tools
  ```

---

## 3. Conectando via Arquivo de Configuração

Caso prefira configurar via arquivos JSON declarativos para o **Claude**, **Cursor**, **Zed** ou **Cline**:

### A. Claude Desktop / Claude Code (`~/.claude.json` ou `claude_desktop_config.json`)

Edite o seu arquivo de configuração (no Linux: `~/.config/Claude/claude_desktop_config.json` ou `~/.claude.json`):

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/home/desktop/Documents/projects/brain_tools/target/release/brain-tools",
      "args": [
        "--root-dir", "/home/desktop/Documents/projects/seu_repositorio",
        "--log-level", "info"
      ],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

### B. Cursor (`.cursor/mcp.json` na raiz do seu projeto)

Crie ou edite o arquivo `.cursor/mcp.json` no repositório em que estiver trabalhando:

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/home/desktop/Documents/projects/brain_tools/target/release/brain-tools",
      "args": [
        "--root-dir", "."
      ]
    }
  }
}
```

### C. VS Code / Cline (`cline_mcp_settings.json`)

No painel de configurações de MCP do plugin Cline:

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/home/desktop/Documents/projects/brain_tools/target/release/brain-tools",
      "args": [
        "--root-dir", "${workspaceFolder}"
      ],
      "disabled": false,
      "autoApprove": [
        "brain_search",
        "brain_context_graph",
        "brain_impact_check"
      ]
    }
  }
}
```

---

## 4. Tabela de Argumentos e Variáveis de Ambiente

O binário aceita tanto flags de linha de comando quanto variáveis de ambiente:

| Argumento CLI | Variável de Ambiente | Padrão | Descrição |
|---|---|---|---|
| `-r`, `--root-dir` | `BRAIN_ROOT_DIR` | `.` | Diretório raiz do repositório a indexar e observar. |
| `-t`, `--threads` | `BRAIN_THREADS` | *Auto (CPUs)* | Número de threads para escaneamento. |
| `--log-level` | `RUST_LOG` | `info` | Nível de log emitido no `stderr` (`error`, `warn`, `info`, `debug`). |
| `--default-max-results` | `BRAIN_MAX_RESULTS` | `5` | Quantidade máxima de recortes cirúrgicos retornados. |

---

## 5. Como o Agente Usa as Ferramentas no Dia a Dia

Uma vez conectado, o Claude usará automaticamente as seguintes ferramentas conforme a necessidade:

1. **`brain_search`:** Quando você pedir *"Onde fica a validação de token JWT?"*, o Claude chama o `brain_search` passando a query e palavras-chave. O servidor devolve a função ou classe inteira em Rust ou Kotlin com numeração de linhas e assinatura, sem ler o arquivo inteiro.
2. **`brain_context_graph`:** Quando você perguntar *"Quem consome a função `verify_token`?"*, o Claude consulta o grafo e obtém as chamadas de entrada e dependências em forma de esqueleto enxuto.
3. **`brain_impact_check`:** Antes de refatorar uma função ou struct, o Claude roda a checagem de impacto para listar quais outros arquivos e rotinas serão quebrados caso a assinatura seja modificada.
