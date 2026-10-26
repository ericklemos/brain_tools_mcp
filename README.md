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

4. **`brain_exec` (Build e Testes Resumidos):**
   - Roda `build`, `check`, `test` ou `clippy` — um enum fechado, nunca uma string de comando.
   - Parseia `--message-format=json` do cargo: erros agrupados por `(código, arquivo)`, span e linha-fonte vindos do próprio compilador. Warnings viram contagem por código.
   - Em `test`, a saída do libtest é fatiada por `---- <teste> stdout ----`: o bloco do assert fica intacto, o miolo é omitido.
   - Cache por hash do conteúdo indexado + estado dos manifests: repetir sem mudar nada devolve uma linha.
   - Um build com 40 erros sai em ~1.300 caracteres, com o restante recuperável por `brain_peek`.

5. **`brain_peek` (Recuperação de Resposta Truncada):**
   - Toda ferramenta respeita `budget_tokens`: o excedente sai do contexto e fica recuperável por um handle `h_xxxxxxxx` (TTL de 30 min), paginável por `offset_chars`.
   - O envelope de truncamento anuncia quantas seções ficaram de fora — a resposta nunca "parece completa" sem ser.

6. **`brain_read` (Leitura Cirúrgica):**
   - Sem `symbol`: devolve o esqueleto do arquivo (uma seção por símbolo, só assinatura). Com `symbol`: devolve só o corpo daquele símbolo, com a linguagem certa no fence.
   - Resolve o caminho contra `--root-dir` e recusa traversal (`../../etc/passwd`) em vez de ler a máquina.

7. **`brain_map` (Mapa do Repositório):**
   - Um arquivo por seção, com os símbolos e assinaturas, ordenado por fan-in (quantas chamadas o arquivo recebe).
   - É a resposta para "onde eu estou?" sem abrir nenhum arquivo; `path` restringe a um subtree.

8. **`brain_assert` (Perguntas Sim/Não):**
   - `exists`, `public` e `used_outside` respondem `SIM`/`NÃO` com a evidência em uma linha — o símbolo existe, é API pública (não `pub(crate)`), ou tem chamadas de outro arquivo/módulo.
   - Encerra o ciclo "achei que era usado, deixa eu grepar para confirmar".

9. **`brain_diff` (Diff com Impacto):**
   - Parseia `git diff --unified=0`, mapeia as linhas alteradas para símbolos do grafo e devolve: símbolos alterados, chamadores **fora** dos arquivos alterados e testes que dependem deles.
   - `include: ["impact", "tests"]` permite pedir só uma das seções quando o custo do teste não importa.

10. **`brain_why` (Histórico de Símbolo):**
    - Roda `git log -L` nas linhas exatas do símbolo e devolve commits (hash, autor, data, assunto) — sem despejar o patch.
    - Responde "por que este código é assim?" no lugar de abrir o histórico do arquivo inteiro.

11. **`brain_ledger` (Memória Durável):**
    - Fatos e decisões do projeto em `.brain/ledger.jsonl` (append-only), deduplicados por hash do texto normalizado.
    - `record`, `list` e `search` — a decisão não precisa ser redescoberta na próxima sessão.

12. **Sincronização Incremental em Segundo Plano:**
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
snippet, os testes em `crates/brain-engine/src/llama_validator.rs` cobrem essa invariante:

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
| `--model-path`, `-m` | — | Caminho do GGUF. Sem ele, opera em modo léxico/AST (fallback). |
| `--gpu-layers` | `33` | Camadas offloaded para a GPU. |
| `--default-min-confidence` | `0.65` | Limiar do modelo semântico. **Só se aplica com o GGUF carregado** — no modo léxico o score é ranking relativo e não há corte. |
| `--default-max-results` | `5` | Máximo de recortes retornados. |
| `--default-budget-tokens` | `2000` | Teto aproximado de tokens por resposta; o excedente vai para `brain_peek`. |
| `--handle-ttl-secs` | `1800` | Tempo que uma resposta truncada continua recuperável. |
| `--handle-max-bytes` | `67108864` | Teto de memória do store de handles (64 MB). |
| `--exec-timeout-secs` | `300` | Mata o `brain_exec` após N segundos e devolve o parcial. |
| `--exec-disabled` | `false` | Não registra `brain_exec`, para hosts onde spawn de processo não é aceitável. |
| `--log-level` | `info` | Filtro de log (sempre em `stderr`, nunca no stdout do protocolo). |
| `--threads`, `-t` | — | Threads de CPU para busca e inferência. |

Todas aceitam variável de ambiente equivalente (`BRAIN_ROOT_DIR`, `BRAIN_GPU_LAYERS`,
`BRAIN_MIN_CONFIDENCE`, `BRAIN_MAX_RESULTS`, `BRAIN_BUDGET_TOKENS`, `RUST_LOG`).
Rode `brain-tools --help` para a lista exata do binário instalado.

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

### Ensinar o agente a confiar no brain-tools

O MCP por si só não impede o agente de reabrir com `Read` o arquivo que a busca já
devolveu. São duas camadas, com poderes diferentes: **instrução** orienta, **hook**
sobrepõe (o harness recusa a chamada). O bloco pronto para colar em
`~/.claude/CLAUDE.md` e o hook estão em 👉 [**docs/hooks.md**](docs/hooks.md).

Em resumo, a instrução precisa dizer três coisas:

```markdown
## brain-tools é a fonte de verdade

O resultado de `brain_search` / `brain_context_graph` / `brain_impact_check` **é** a
leitura do código: trecho verbatim, com `path:start-end`. Trate-o como já lido e não
chame `Read` no mesmo arquivo para conferir. Score baixo não é motivo de desconfiança:
ele ordena candidatas entre si, não mede veracidade. `Read` só é legítimo para imports
e contexto fora da faixa devolvida, para o arquivo que será editado (`Edit` exige
leitura prévia) ou quando a resposta declarar corte. Se a busca não respondeu, refine
a busca — não abra o arquivo.
```

Se uma chamada MCP falhar, o agente deve dizer isso explicitamente em vez de
silenciosamente seguir com grep — a degradação da busca merece ser sinalizada.

---

## 🖥️ Configurar em outro PC

Passo a passo do zero. Troque `/caminho/do/brain_tools` pelo caminho real da máquina.

### 1. Dependências e build

```bash
git clone <repo> brain_tools && cd brain_tools

# Ubuntu/Debian — tree-sitter e llama.cpp precisam de toolchain C:
sudo apt install build-essential cmake clang libclang-dev   # libclang-dev: bindgen

cargo build --release                 # CPU
cargo build --release --features cuda # GPU NVIDIA
```

O binário fica em `target/release/brain-tools`. **Refaça o build sempre que puxar
código novo** — nenhum cliente avisa que está rodando um binário velho (sintoma típico:
faltam ferramentas no `tools/list`).

### 2. Modelo GGUF (opcional)

```bash
./scripts/download_model.sh    # ~806 MB em models/
```

Sem o modelo o servidor roda em modo léxico/AST — funciona, mas `--default-min-confidence`
deixa de ter efeito.

### 3. Registrar o MCP

```bash
claude mcp add brain-tools \
  /caminho/do/brain_tools/target/release/brain-tools \
  --model-path /caminho/do/brain_tools/models/vllm-d3-edge-Q8_0.gguf \
  --gpu-layers 33
```

**Não passe `--root-dir`**: o Claude Code inicia o servidor com cwd igual ao diretório do
projeto, e o padrão `.` passa a indexar o repositório em que você está aberto — um único
registro serve para todos os projetos.

Confirme as ferramentas expostas:

```bash
claude mcp list
```

Devem aparecer `brain_search`, `brain_context_graph`, `brain_impact_check`, `brain_peek`,
`brain_exec`, `brain_read`, `brain_map`, `brain_assert`, `brain_diff`, `brain_why` e
`brain_ledger` (`brain_exec` só não aparece com `--exec-disabled`). Se faltar alguma, o
binário é antigo.

### 4. Instrução — `~/.claude/CLAUDE.md`

Cole o bloco "brain-tools é a fonte de verdade" de [docs/hooks.md](docs/hooks.md).
É ele que autoriza o agente a parar de verificar o que já recebeu.

### 5. Hook anti-releitura — `~/.claude/settings.json`

O hook é a única camada que **sobrepõe** o comportamento: o harness recusa a releitura
em vez de apenas desaconselhá-la.

```bash
mkdir -p ~/.claude/hooks
cp /caminho/do/brain_tools/scripts/brain-read-once.py ~/.claude/hooks/
chmod +x ~/.claude/hooks/brain-read-once.py
```

Adicione ao `~/.claude/settings.json` (use o caminho **absoluto** do script; caminho
relativo não resolve):

```json
{
  "permissions": {
    "allow": [
      "mcp__brain-tools__brain_search",
      "mcp__brain-tools__brain_context_graph",
      "mcp__brain-tools__brain_impact_check",
      "mcp__brain-tools__brain_peek",
      "mcp__brain-tools__brain_exec",
      "mcp__brain-tools__brain_read",
      "mcp__brain-tools__brain_map",
      "mcp__brain-tools__brain_assert",
      "mcp__brain-tools__brain_diff",
      "mcp__brain-tools__brain_why",
      "mcp__brain-tools__brain_ledger"
    ]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "python3 -I /home/SEU_USUARIO/.claude/hooks/brain-read-once.py",
            "timeout": 10
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "python3 -I /home/SEU_USUARIO/.claude/hooks/brain-read-once.py",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

O hook **não** bloqueia a primeira leitura de um arquivo — é ela que habilita o `Edit`.
Ele bloqueia só a repetição de uma faixa já lida, com o arquivo inalterado desde então.
Detalhes e tabela de casos em [docs/hooks.md](docs/hooks.md).

### 6. Reiniciar e verificar

`CLAUDE.md` e hooks são lidos na inicialização: **reinicie o Claude Code**.

```bash
claude mcp list                      # brain-tools deve estar connected
```

Numa sessão nova, peça uma busca conceitual e confira a resposta:

- começa com `N de M candidatas casaram com a query` e a linha `Trecho verbatim`;
- **não** traz mais `Confiança: NN%` (o número era ranking relativo, não probabilidade);
- se o agente tentar reler o mesmo arquivo, o hook responde com a mensagem de bloqueio.

### Diagnóstico rápido

| Sintoma | Causa provável |
|---|---|
| `Connection closed` logo após conectar | `--root-dir` inexistente, ou binário quebrado/incompatível |
| Menos ferramentas que o esperado | binário antigo — refaça `cargo build --release` |
| Busca sempre vazia | servidor indexando outro diretório (cwd errado) |
| Hook não dispara | caminho relativo no `settings.json`, script sem permissão, ou sessão não reiniciada |
| `brain_exec` não aparece | `--exec-disabled` ativo |
| Agente ainda relê | instrução ausente no `CLAUDE.md` e/ou hook não registrado |

---

## 🧪 Suíte de Testes

Para rodar todos os testes de unidade, integração e smoke tests:
```bash
cargo test
```

---

## 🧩 Estrutura do Workspace

O projeto é um workspace Cargo em microkernel: um núcleo estável e cada ferramenta MCP
como um plugin registrado nele.

```
crates/
  brain-kernel/   protocol (JSON-RPC), tipos AST, AstChunker, Config, Registry/ToolPlugin,
                  budget/Section, ResponseStore, brain_peek
  brain-search/   scanner léxico + brain_search
  brain-graph/    grafo de código, watcher, brain_context_graph, brain_impact_check
  brain-exec/     runner do cargo, parsers (JSON e libtest), cache + brain_exec
  brain-engine/   inferência GGUF (llama-cpp-2 isolado aqui)
  brain-tools/    binário, monta o Registry em main.rs e re-exporta os caminhos antigos
```

Adicionar uma ferramenta é criar o tipo no crate dela e uma linha de `register` em
`main.rs` — não há mais um `match` de nomes nem uma lista de schemas a editar.

As features `cuda`, `vulkan` e `metal` propagam `brain-tools → brain-engine → llama-cpp-2`:

```bash
cargo build --release --features cuda   # build GPU
```

`brain-tools` mantém um módulo de compatibilidade (`crates/brain-tools/src/lib.rs`) que
re-exporta `brain_tools::search`, `::graph`, `::engine` e `::mcp`, então código que usa a
API antiga continua compilando.
