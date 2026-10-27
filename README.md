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
   - **Uma chamada é uma aresta.** A query de chamadas casa o mesmo nó por mais de um padrão (um carrega o `receiver`, outro o `call_path`); os padrões são fundidos pelo span do nome antes da resolução, então não há aresta repetida nem chamada contada duas vezes na confiança do grafo.
   - Quando o alvo é um arquivo inteiro, as seções vazias viram **uma** linha de manchete (`N sem chamadas de entrada, M sem consumo externo`) em vez de se repetirem símbolo a símbolo.

3. **`brain_impact_check` (Análise de Impacto Reverso):**
   - Percorre dependências reversas transitivas no grafo para apontar todos os pontos do repositório que quebram caso uma assinatura seja modificada.
   - **Resolução por escopo, não por nome.** Uma chamada é ligada à definição pelo mesmo arquivo → `use` explícito → caminho da própria chamada (`crate::a::Thing::new`) → mesmo crate → nome global sem ambiguidade. Um nome ambíguo sem evidência local **não gera aresta**: aresta faltando é recuperável, aresta errada contamina todas as ferramentas.
   - Métodos resolvem por tipo do receptor (`x.build()` → `GraphBuilder::build`), a partir de `self`, do tipo do `impl`, de parâmetros tipados e de `let x = Type::new(…)`.
   - O chamador é sempre o nó **mais interno** que contém a linha.
   - O relatório abre com a confiança do grafo (`> Confiança do grafo: 32% das 3712 chamadas resolvidas`): a ferramenta diz o quanto não sabe, em vez de parecer completa. "Nenhum chamador encontrado" nunca é apresentado como "não há chamadores".

4. **`brain_exec` (Build e Testes Resumidos):**
   - Roda `build`, `check`, `test` ou `clippy` — um enum fechado, nunca uma string de comando.
   - Parseia `--message-format=json` do cargo: erros agrupados por `(código, arquivo)`, span e linha-fonte vindos do próprio compilador. Warnings viram contagem por código.
   - Em `test`, a saída do libtest é fatiada por `---- <teste> stdout ----`: o bloco do assert fica intacto, o miolo é omitido.
   - Cache por hash do conteúdo indexado + estado dos manifests: repetir sem mudar nada devolve uma linha.
   - Um build com 40 erros sai em ~1.300 caracteres, com o restante recuperável por `brain_peek`.

5. **`brain_shell` (Shell Explícito):**
   - Executa um comando em `bash -lc` com cwd no root e devolve stdout, stderr e código de saída, dentro do mesmo contrato de orçamento.
   - Substitui o `shell` nativo para git, formatadores e ferramentas que o enum do `brain_exec` não cobre. Não é um sandbox: `--shell-disabled` remove a ferramenta.

6. **`brain_peek` (Recuperação de Resposta Truncada):**
   - Toda ferramenta respeita `budget_tokens`: o excedente sai do contexto e fica recuperável por um handle `h_xxxxxxxx` (TTL de 30 min), paginável por `offset_chars`.
   - O envelope de truncamento anuncia quantas seções ficaram de fora — a resposta nunca "parece completa" sem ser.

7. **`brain_read` (Leitura Cirúrgica):**
   - Sem `symbol`: devolve o esqueleto do arquivo (uma seção por símbolo, só assinatura). Com `symbol`: devolve só o corpo daquele símbolo, com a linguagem certa no fence.
   - Resolve o caminho contra `--root-dir` e recusa traversal (`../../etc/passwd`) em vez de ler a máquina.

8. **`brain_edit` (Edição Cirúrgica por Span):**
   - Edita o corpo de um símbolo existente por nome — `path`, `symbol`, `expected_hash` e `new_body` —, nunca por âncora de texto nem arquivo inteiro.
   - O `expected_hash` é o blake3 que o `brain_read` imprime no cabeçalho; se o arquivo mudou desde a leitura, a edição é recusada em vez de sobrescrever trabalho alheio.
   - Resolve o span na AST (inclusive métodos dentro de `impl`), recusa símbolo ambíguo ou item sem corpo e re-parseia antes de escrever: sintaxe inválida não toca no disco.
   - Escrita atômica (`temp` no mesmo diretório + `rename`), releitura pós-escrita e verificação de hash/span/corpo. Sem formatação automática; o retry de uma resposta perdida responde "já aplicada" sem reescrever.

9. **`brain_write` (Arquivo Inteiro):**
   - Cria ou substitui um arquivo de texto sob o root — configs, docs, módulos novos —, com escrita atômica e releitura de verificação.
   - Sobrescrever exige o blake3 atual em `expected_hash` (ou `overwrite: true`); caminhos são canonicalizados e symlinks para fora do root são recusados.

10. **`brain_patch` (Diff Unificado):**
   - Aplica a saída de `git diff` (arquivos novos, remoções, trechos fora de símbolos) com `git apply --check` antes de escrever.
   - Recusa caminhos fora do root, `.git/` e criação de symlink; `check_only` valida sem tocar na árvore e `reverse: true` desfaz.

11. **`brain_map` (Mapa do Repositório):**
   - Um arquivo por seção, com os símbolos e assinaturas, ordenado por fan-in (quantas chamadas o arquivo recebe).
   - Carrega a mesma linha de confiança do grafo: a ordenação por centralidade só vale o que a resolução de chamadas vale.
   - É a resposta para "onde eu estou?" sem abrir nenhum arquivo; `path` restringe a um subtree.

12. **`brain_assert` (Perguntas Sim/Não):**
   - `exists`, `public` e `used_outside` respondem `SIM`/`NÃO` com a evidência em uma linha — o símbolo existe, é API pública (não `pub(crate)`), ou tem chamadas de outro arquivo/módulo.
   - Encerra o ciclo "achei que era usado, deixa eu grepar para confirmar".

13. **`brain_diff` (Diff com Impacto):**
   - Parseia `git diff --unified=0`, mapeia as linhas alteradas para símbolos do grafo e devolve: símbolos alterados, chamadores **fora** dos arquivos alterados e testes que dependem deles.
   - `include: ["impact", "tests"]` permite pedir só uma das seções quando o custo do teste não importa.

14. **`brain_why` (Histórico de Símbolo):**
    - Roda `git log -L` nas linhas exatas do símbolo e devolve commits (hash, autor, data, assunto) — sem despejar o patch.
    - Responde "por que este código é assim?" no lugar de abrir o histórico do arquivo inteiro.

15. **`brain_ledger` (Memória Durável):**
    - Fatos e decisões do projeto em `.brain/ledger.jsonl` (append-only), deduplicados por hash do texto normalizado.
    - `record`, `list` e `search` — a decisão não precisa ser redescoberta na próxima sessão.

16. **Sincronização Incremental em Segundo Plano:**
    - Watcher assíncrono via `notify` e `blake3`.
    - Ao salvar um arquivo no editor, o grafo atualiza apenas os nós modificados em milissegundos sem reindexar o projeto todo.

17. **`brain-bench` (Medição de Economia de Tokens):**
   - Não é uma ferramenta MCP: é um harness que sobe o binário real por stdio e replaya tarefas fixas de `bench/tasks.json`, comparando os tokens entregues com o custo de reler os arquivos que a resposta cobre.
   - Toda tarefa declara seu baseline `naive` à mão — o servidor nunca infere o próprio custo contrafactual.
   - Guarda de regressão: `cargo run -p brain-bench` falha quando uma tarefa (ou o schema de `tools/list`) cresce mais que o teto configurado sobre `bench/baseline.json`.
   - Detalhes em 👉 [**docs/bench.md**](docs/bench.md).

18. **Ledger de Entrega (não pagar duas vezes pelo mesmo trecho):**
   - O daemon lembra o que **esta sessão** já entregou: `brain_read` e `brain_search` guardam o texto e o blake3 do arquivo no momento da entrega.
   - Pedir o mesmo símbolo de novo, com o arquivo intacto, devolve uma linha apontando para um handle vivo em vez do corpo; se o arquivo mudou, devolve **só o diff** do símbolo.
   - Um recorte (corpo truncado) nunca vira ponteiro nem diff: o corpo inteiro volta com o motivo escrito. `repeat: "allow"` força o corpo, e a terceira repetição seguida também.
   - Ligado por padrão; `--delivery-ledger false` reproduz o comportamento anterior. Desenho em [docs/superpowers/specs/2026-10-27-delivery-ledger-design.md](docs/superpowers/specs/2026-10-27-delivery-ledger-design.md).

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
| `--delivery-ledger` | `true` | Lembra o que a sessão já entregou e aponta para isso em vez de reenviar (`false` desliga). |
| `--delivery-entries` | `512` | Quantos símbolos entregues o ledger guarda antes de despejar o mais antigo. |
| `--delivery-max-bytes` | `8388608` | Teto de bytes de texto guardado para ponteiros e diffs. |
| `--exec-timeout-secs` | `300` | Mata o `brain_exec` após N segundos e devolve o parcial. |
| `--exec-disabled` | `false` | Não registra `brain_exec`, para hosts onde spawn de processo não é aceitável. |
| `--shell-timeout-secs` | `120` | Mata um comando do `brain_shell` após N segundos. |
| `--shell-disabled` | `false` | Não registra `brain_shell`; use quando "somente leitura" precisa ser garantia, não comportamento. |
| `--log-level` | `info` | Filtro de log (sempre em `stderr`, nunca no stdout do protocolo). |
| `--threads`, `-t` | — | Threads de CPU para busca e inferência. |

Todas aceitam variável de ambiente equivalente (`BRAIN_ROOT_DIR`, `BRAIN_GPU_LAYERS`,
`BRAIN_MIN_CONFIDENCE`, `BRAIN_MAX_RESULTS`, `BRAIN_BUDGET_TOKENS`, `RUST_LOG`).
Rode `brain-tools --help` para a lista exata do binário instalado.

> ⚠️ **`--exec-timeout-secs` não é o timeout do MCP.** Ele limita só o processo filho do
> `brain_exec` (300 s por padrão). O tempo que o **cliente** espera por uma resposta MCP
> é outro controle, configurado do lado do cliente — veja
> [Timeouts](#timeouts-quanto-esperar-do-d3-edge-e-como-não-ser-cortado).

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

### Timeouts: quanto esperar do `d3-edge` e como não ser cortado

O `d3-edge` é um modelo de decisão de 0.7B, **não** um gerador de texto: cada validação
semântica é um forward pass de logits, não uma geração de tokens. O custo real medido
nesta máquina (Q8_0, `--gpu-layers 33`, projeto de 932 arquivos / 138 mil linhas) é:

| Etapa | Tempo medido |
|---|---|
| `initialize` (handshake, servidor frio) | 1,66 s |
| `initialize` (servidor já carregado) | 0,82 s |
| `tools/list` | 0,00 s |
| `brain_search` (primeira chamada) | 0,12 s |
| `brain_search` (segunda chamada) | 0,60 s |

O carregamento do GGUF acontece numa **thread de fundo** (`crates/brain-engine/src/actor.rs`),
então ele nunca bloqueia o handshake — o `initialize` responde antes de o modelo estar
pronto. Por isso timeout de inicialização **não** é o problema típico deste servidor. O
que pode estourar é uma chamada individual quando o `n_ubatch` é redimensionado para um
snippet grande (ver [Modelos não-causais](#modelos-não-causais-atenção-vllm-d3-edge)) ou
quando várias instâncias disputam a mesma GPU.

**Nunca configure 10 minutos como teto global de chamada.** Os defaults do cliente são
maiores que isso, e um `600000` fixo *reduziria* a margem em vez de aumentá-la. Valores
reais do Claude Code 2.1.x:

| Controle | Default | Escopo |
|---|---|---|
| `MCP_TIMEOUT` (ms) | **30 000** | Inicialização/conexão do servidor. |
| watchdog de silêncio em stdio | **30 min** | Por chamada: quanto tempo a ferramenta pode ficar sem emitir nada. |
| campo `timeout` (ms) no servidor | herda o idle acima | Wall-clock **duro** por chamada; **substitui** o idle global. Valores < 1000 ms são ignorados. |

Ou seja: o caminho certo é **não mexer** no idle (30 min já cobre com folga um forward
pass de 1 s) e só subir `MCP_TIMEOUT` se o handshake falhar de fato. Para exigir um teto
duro maior por chamada, use o campo `timeout` — em milissegundos — e mantenha-o **acima**
de 30 min se a intenção for afrouxar, não apertar:

```json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/brain-tools/target/release/brain-tools",
      "args": [
        "--model-path", "/caminho/absoluto/brain-tools/models/vllm-d3-edge-Q8_0.gguf",
        "--gpu-layers", "33"
      ],
      "env": { "RUST_LOG": "info", "MCP_TIMEOUT": "300000" },
      "timeout": 3600000
    }
  }
}
```

`"timeout": 3600000` = 1 hora de teto duro por chamada. É seguro: um forward pass de 1 s
nunca chega perto disso, e se a GPU travar o agente recebe um erro em vez de esperar
para sempre. `MCP_TIMEOUT=300000` dá 5 min ao handshake — folga absurda para um
`initialize` de 1,66 s, sem penalizar o usuário quando a máquina está carregada.

Para aumentar **todos** os servidores de uma vez, use as variáveis de ambiente globais em
vez de editar cada registro: `MCP_TIMEOUT` (startup) e `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
(idle por chamada; `0` desativa).

> ⚠️ **Timeout não é o gargalo deste modelo.** Se a busca estiver demorando segundos em
> vez de milissegundos, o sintoma real é contenção de GPU: cada sessão do Claude Code
> inicia **sua própria** instância do servidor, e cada uma carrega ~500 MB de modelo na
> VRAM. Confira com `nvidia-smi --query-compute-apps=pid,used_memory --format=csv` e
> `pgrep -cf "brain-tools --model-path"`. Aumentar timeout apenas transforma "corta
> cedo" em "espera muito" — não devolve a VRAM.

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

No **OpenCode V2** a trava não é hook, é permissão: dá para negar `read`, `edit`,
`glob`, `grep` e `shell` e deixar o agente falar com o projeto só pelo MCP. A
receita completa — incluindo o que essa trava custa e como o `brain_edit` fecha o ciclo
de escrita por símbolo — está em 👉 [**docs/opencode.md**](docs/opencode.md),
com um [`opencode.example.json`](docs/opencode.example.json) pronto para copiar.

Em resumo, a instrução precisa dizer três coisas:

```markdown
## brain-tools é a fonte de verdade

O resultado de `brain_search` / `brain_context_graph` / `brain_impact_check` **é** a
leitura do código: trecho verbatim, com `path:start-end`. Trate-o como já lido e não
chame `Read` no mesmo arquivo para conferir. Score baixo não é motivo de desconfiança:
ele ordena candidatas entre si, não mede veracidade. `Read` só é legítimo para imports
e contexto fora da faixa devolvida, para um símbolo que você ainda não viu ou quando a
resposta declarar corte. Para editar um símbolo já visto, use `brain_edit` com o hash
do `brain_read` e o `new_body` completo — sem `Read` + `Edit`; para arquivos inteiros e
diffs, use `brain_write` / `brain_patch`. Se a busca não respondeu, refine a busca —
não abra o arquivo.
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
MCP_TIMEOUT=300000 claude mcp add brain-tools \
  /caminho/do/brain_tools/target/release/brain-tools \
  --model-path /caminho/do/brain_tools/models/vllm-d3-edge-Q8_0.gguf \
  --gpu-layers 33
```

`MCP_TIMEOUT=300000` (5 min) é folga deliberada no handshake. O default é 30 s e o
`initialize` real leva ~1,7 s, então isso é margem para máquina carregada — não uma
necessidade. Se você forçar um teto duro por chamada, use o campo `timeout` (ms) no
registro e **não** desça abaixo de 30 min, que é o default de silêncio em stdio. Detalhes
em [Timeouts](#timeouts-quanto-esperar-do-d3-edge-e-como-não-ser-cortado).

**Não passe `--root-dir`**: o Claude Code inicia o servidor com cwd igual ao diretório do
projeto, e o padrão `.` passa a indexar o repositório em que você está aberto — um único
registro serve para todos os projetos.

Confirme as ferramentas expostas:

```bash
claude mcp list
```

Devem aparecer `brain_search`, `brain_context_graph`, `brain_impact_check`, `brain_peek`,
`brain_exec`, `brain_shell`, `brain_read`, `brain_edit`, `brain_write`, `brain_patch`,
`brain_map`, `brain_assert`, `brain_diff`, `brain_why` e `brain_ledger` (`brain_exec` só
não aparece com `--exec-disabled`; `brain_shell` com `--shell-disabled`). Se faltar alguma, o
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
      "mcp__brain-tools__brain_shell",
      "mcp__brain-tools__brain_read",
      "mcp__brain-tools__brain_edit",
      "mcp__brain-tools__brain_write",
      "mcp__brain-tools__brain_patch",
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
| Servidor `✗ Failed to connect` | handshake acima de `MCP_TIMEOUT` (30 s) — suba a variável |
| Ferramentas do brain ausentes numa sessão só | sessão iniciada antes do servidor ser registrado; reinicie |
| Busca leva segundos | contenção de GPU: várias sessões, uma instância do servidor cada |
| Busca correta mas lenta na 1ª chamada | forward pass frio do `d3-edge`; ~1 s, não é erro |

---

## 🧪 Suíte de Testes

Para rodar todos os testes de unidade, integração e smoke tests:
```bash
cargo test
```

## 📉 Medindo a economia de tokens

A pergunta "isto economiza tokens?" tem número, não opinião:

```bash
cargo build --release
cargo run -p brain-bench -- --update   # grava bench/baseline.json
cargo run -p brain-bench               # compara e falha se houver regressão
```

O relatório mostra, por tarefa, os tokens entregues (`brain`), o custo de um
`Read` que cobriria a mesma resposta (`naïve`) e o fator entre eles — inclusive
quando o fator fica abaixo de `1,0x`, que é o servidor custando **mais** que
abrir o arquivo. O `tools/list` é medido à parte porque é pago a cada
requisição. Regras, opções e como adicionar tarefas em
👉 [**docs/bench.md**](docs/bench.md).

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
  brain-bench/    harness de economia de tokens (cliente MCP + baseline), fora do servidor
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
