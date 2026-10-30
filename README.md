# brain-tools

Servidor MCP local, escrito em Rust, para **ler, entender e editar código com menos tokens**. Em vez de despejar arquivos inteiros, o `brain-tools` devolve o recorte que importa: o corpo de um símbolo, o esqueleto de um arquivo, quem chama quem, o impacto de uma mudança, o erro agrupado de um build.

O objetivo é a economia de tokens, mas não só ela. A ferramenta também faz
**seleção inteligente de conteúdo**: repassa ao agente o que de fato responde à
pergunta — o símbolo certo, o chamador que quebra, o erro que importa — e deixa
o resto fora do contexto. Menos tokens é consequência de um contexto melhor
filtrado, mais bem construído e mais centrado, que reduz distração e erro de
atenção do modelo.

O contrato central do projeto:

> O resultado de uma ferramenta **é** a leitura do arquivo: trecho verbatim, com `path:start-end` e hash. Não é um resumo que precisa ser conferido depois.

---

## Sumário

- [O que é](#o-que-é)
- [Requisitos e instalação](#requisitos-e-instalação)
- [Configuração](#configuração)
- [Perfis de ferramentas](#perfis-de-ferramentas)
- [Contrato de resposta](#contrato-de-resposta)
- [Ferramentas](#ferramentas)
- [Guarda do shell](#guarda-do-shell)
- [Bloqueio das ferramentas alternativas](#bloqueio-das-ferramentas-alternativas)
- [Fluxo de trabalho recomendado](#fluxo-de-trabalho-recomendado)
- [Desenvolvimento](#desenvolvimento)
- [Solução de problemas](#solução-de-problemas)
- [Segurança](#segurança)

---

## O que é

O `brain-tools` é um servidor [MCP](https://modelcontextprotocol.io/) que fala JSON-RPC por `stdio`. Ele indexa um diretório de projeto, constrói um grafo de chamadas com tree-sitter e responde às ferramentas abaixo.

Características principais:

- **Busca cirúrgica** com ripgrep + AST para Rust e Kotlin.
- **Grafo de chamadas** com resolução de escopo entre arquivos e crates.
- **Edição por span de AST**, com hash do arquivo para evitar escrita em conteúdo obsoleto.
- **Build/testes agrupados**, com cache e parser de diagnóstico do Cargo.
- **Contexto filtrado**: entrega o recorte que responde à pergunta — símbolo, chamador, impacto, erro — em vez do arquivo inteiro.
- **Orçamento de tokens** por resposta; o excedente fica recuperável via `brain_peek`.
- **Ledger de entregas**: o mesmo corpo não é enviado duas vezes na mesma sessão.
- **Guarda do shell**: recusa comandos que outra ferramenta já cobre.
- **Leitura da web** com `brain_fetch`, `brain_websearch` e `brain_research`.

O projeto tem sete crates:

| Crate | Responsabilidade |
|---|---|
| `brain-kernel` | Configuração, protocolo MCP, registro de plugins, orçamento, store de handles, ledger e AST compartilhada. |
| `brain-search` | Scanner lexical (ripgrep), ranking e montagem dos trechos com tree-sitter. |
| `brain-graph` | Grafo de código, resolução de escopo e ferramentas de leitura, mapa, impacto, edição, patch e histórico. |
| `brain-exec` | Execução de `cargo`, parser de erros/testes, cache, guarda e log de uso do shell. |
| `brain-web` | Fetch de URLs, busca em provedores gratuitos e pesquisa com evidências. |
| `brain-tools` | Binário `brain-tools`, servidor MCP e composição de tudo. |
| `brain-bench` | Benchmarks de custo e comparação com leitura ingênua. |

---

## Requisitos e instalação

- Rust stable (edição 2021).
- Linux ou macOS. O projeto usa `bash -lc` no `brain_shell`; em Windows, use WSL.
- Nenhum modelo local, GPU ou API paga é necessário. O ranking é lexical.

~~~sh
git clone <url-do-repositorio>
cd brain_tools
cargo build --release
./target/release/brain-tools --version
~~~

O binário fica em `target/release/brain-tools`. Opcionalmente, coloque-o no `PATH`:

~~~sh
install -m 755 target/release/brain-tools ~/.local/bin/brain-tools
~~~

O servidor escreve logs **somente em stderr**, para não corromper o JSON-RPC em stdout.

---

## Configuração

### OpenCode

O arquivo `opencode.json` na raiz do projeto registra o servidor:

~~~json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "brain-tools": {
        "type": "local",
        "command": [
          "/caminho/absoluto/para/brain_tools/target/release/brain-tools",
          "--root-dir",
          "/caminho/absoluto/para/o/projeto"
        ],
        "environment": {
          "RUST_LOG": "info"
        }
      }
    }
  }
}
~~~

Pontos importantes:

- Use **caminho absoluto** para o binário e para `--root-dir`. O processo do MCP pode iniciar com um diretório de trabalho diferente do projeto.
- Sem `--root-dir`, o padrão é `.` (o diretório de trabalho do processo).
- Todas as ferramentas recebem caminhos **relativos ao root**.
- As ferramentas aparecem no OpenCode como `brain-tools_brain_search`, `brain-tools_brain_read`, etc.

Para conferir o registro:

~~~sh
opencode mcp list
~~~

Se o servidor não aparecer, veja [Solução de problemas](#solução-de-problemas).

### Outros clientes MCP

A maioria dos clientes usa o formato `mcpServers`:

~~~json
{
  "mcpServers": {
    "brain-tools": {
      "command": "/caminho/absoluto/para/brain_tools/target/release/brain-tools",
      "args": ["--root-dir", "/caminho/absoluto/para/o/projeto"],
      "env": { "RUST_LOG": "info" }
    }
  }
}
~~~

O servidor não usa rede para funcionar, exceto quando você chama `brain_fetch`, `brain_websearch` ou `brain_research`.

### Opções de linha de comando

Todas as opções também podem ser definidas por variável de ambiente.

| Flag | Variável | Padrão | Descrição |
|---|---|---|---|
| `-r, --root-dir` | `BRAIN_ROOT_DIR` | `.` | Raiz do repositório indexado. |
| `--tools` | `BRAIN_TOOLS` | `full` | Perfil exposto: `full`, `core` ou `read`. |
| `-t, --threads` | `BRAIN_THREADS` | automático | Threads da busca. |
| `--log-level` | `RUST_LOG` | `info` | Filtro de log (`error`, `warn`, `info`, `debug`, `trace`). |
| `--default-min-confidence` | `BRAIN_MIN_CONFIDENCE` | `0.65` | Aceito por compatibilidade; o ranking é lexical e não corta por limiar. |
| `--default-max-results` | `BRAIN_MAX_RESULTS` | `5` | Máximo padrão de resultados do `brain_search`. |
| `--max-validate-candidates` | `BRAIN_MAX_VALIDATE_CANDIDATES` | `24` | Candidatos considerados pelo ranker. |
| `--default-budget-tokens` | `BRAIN_BUDGET_TOKENS` | `2000` | Orçamento aproximado de tokens por resposta. |
| `--handle-ttl-secs` | `BRAIN_HANDLE_TTL_SECS` | `1800` | Tempo que uma resposta truncada fica recuperável. |
| `--handle-max-bytes` | `BRAIN_HANDLE_MAX_BYTES` | `67108864` | Bytes totais do store de handles. |
| `--delivery-ledger` | `BRAIN_DELIVERY_LEDGER` | `true` | Evita reenviar corpos já entregues na sessão. |
| `--delivery-entries` | `BRAIN_DELIVERY_ENTRIES` | `512` | Spans lembrados pelo ledger. |
| `--delivery-max-bytes` | `BRAIN_DELIVERY_MAX_BYTES` | `8388608` | Bytes de texto entregue mantidos para ponteiros e diffs. |
| `--exec-timeout-secs` | `BRAIN_EXEC_TIMEOUT_SECS` | `300` | Tempo máximo de um `brain_exec`. |
| `--exec-disabled` | `BRAIN_EXEC_DISABLED` | `false` | Não registra `brain_exec`. |
| `--shell-timeout-secs` | `BRAIN_SHELL_TIMEOUT_SECS` | `120` | Tempo máximo de um `brain_shell`. |
| `--shell-disabled` | `BRAIN_SHELL_DISABLED` | `false` | Não registra `brain_shell`. |
| `--shell-guard` | `BRAIN_SHELL_GUARD` | `true` | Recusa comandos cobertos por outras ferramentas. |
| `--shell-usage-log` | `BRAIN_SHELL_USAGE_LOG` | `true` | Grava metadados de uso do shell em JSONL. |
| `--shell-usage-log-path` | `BRAIN_SHELL_USAGE_LOG_PATH` | `.brain/shell-usage.jsonl` | Caminho do log, relativo ao root. |
| `--shell-usage-log-full` | `BRAIN_SHELL_USAGE_LOG_FULL` | `false` | Inclui o comando cru e os primeiros 200 caracteres de stdout/stderr. |

Exemplo de configuração restritiva para um host que não deve executar nada:

~~~sh
brain-tools --root-dir /projeto --tools read --exec-disabled --shell-disabled
~~~

---

## Perfis de ferramentas

O perfil controla o que é **registrado** no MCP. Uma ferramenta fora do perfil não existe para o cliente.

| Ferramenta | `full` | `core` | `read` |
|---|:---:|:---:|:---:|
| `brain_search` | sim | sim | sim |
| `brain_read` | sim | sim | sim |
| `brain_read_many` | sim | sim | sim |
| `brain_map` | sim | sim | sim |
| `brain_context_graph` | sim | sim | sim |
| `brain_impact_check` | sim | sim | sim |
| `brain_assert` | sim | — | sim |
| `brain_diff` | sim | — | sim |
| `brain_trace` | sim | — | — |
| `brain_why` | sim | — | sim |
| `brain_edit` | sim | sim | — |
| `brain_write` | sim | sim | — |
| `brain_write_many` | sim | sim | — |
| `brain_patch` | sim | sim | — |
| `brain_exec` | sim | sim | — |
| `brain_shell` | sim | — | — |
| `brain_peek` | sim | sim | sim |
| `brain_ledger` | sim | — | — |
| `brain_fetch` | sim | sim | sim |
| `brain_websearch` | sim | sim | sim |
| `brain_research` | sim | sim | sim |

- **`full`**: tudo. Indicado para o agente que edita o projeto.
- **`core`**: o ciclo de edição (busca, leitura, edição, escrita, patch, exec e web), sem shell e sem ferramentas de auditoria.
- **`read`**: revisão e análise, sem escrita, sem exec e sem shell.

---

## Contrato de resposta

### Orçamento e `brain_peek`

Toda ferramenta aceita `budget_tokens`. Quando a resposta não cabe:

1. O conteúdo principal é entregue.
2. O excedente vai para um store em memória.
3. A resposta inclui um handle no formato `h_xxxxxxxx`.

Para recuperar:

~~~json
{ "handle": "h_xxxxxxxx", "offset_chars": 0, "budget_tokens": 2000 }
~~~

O handle expira em `--handle-ttl-secs` (padrão 30 minutos). Não presuma que ele é eterno.

### Ledger de entregas

Se esta sessão já recebeu o mesmo corpo, a ferramenta devolve um ponteiro em vez do texto de novo. Para forçar a entrega:

~~~json
{ "path": "src/auth.rs", "symbol": "verify_token", "repeat": "allow" }
~~~

`brain_search` também aceita `repeat: "allow"`.

### Hash e escrita segura

`brain_read` imprime no cabeçalho o blake3 do arquivo inteiro (12 caracteres). `brain_edit` e `brain_write` exigem esse hash para sobrescrever. Se o arquivo mudou desde a leitura, a escrita é recusada em vez de aplicar sobre conteúdo obsoleto.

`brain_write` também aceita `overwrite: true` para substituições intencionais e `create_only: true` para não sobrescrever um arquivo existente.

### Caminhos

- Todos os caminhos são relativos ao `--root-dir`.
- `read`, `edit`, `write` e `patch` recusam caminhos fora do root.
- `brain_write` cria diretórios intermediários dentro do root por padrão.

### Busca lexical

O ranking atual é **lexical**, não semântico. O campo `min_confidence` continua aceito por compatibilidade, mas não há corte por probabilidade. Quando a validação semântica não está disponível, a resposta declara isso explicitamente.

---

## Ferramentas

### Visão geral

| Ferramenta | Para quê | Substitui |
|---|---|---|
| `brain_search` | Encontrar código por termos e devolver corpos verbatim. | `grep`, `rg`, `ag`, `ack` |
| `brain_read` | Ler o esqueleto de um arquivo ou o corpo de um símbolo, no worktree ou em `rev:path`. | `cat`, `head`, `tail`, `sed`, `awk`, `cut`, `git cat-file <rev>:<path>` |
| `brain_read_many` | Ler vários arquivos ou símbolos num lote, com um orçamento único. | várias chamadas de `brain_read` |
| `brain_map` | Ver a estrutura do projeto por centralidade. | `find`, `ls`, `tree`, `fd` |
| `brain_context_graph` | Ver quem chama e o que o símbolo consome. | leitura manual de chamadores |
| `brain_impact_check` | Ver o que quebra se a assinatura mudar. | `grep` reverso |
| `brain_assert` | Responder SIM/NÃO sobre existência, visibilidade e uso externo. | `grep` de confirmação |
| `brain_diff` | Ver símbolos alterados, chamadores externos e testes afetados. | `git diff` |
| `brain_trace` | Encontrar o caminho de chamadas entre A e B. | `context_graph` com depth alto |
| `brain_why` | Ver o histórico git de um símbolo. | `git log -L`, `git blame` |
| `brain_edit` | Editar o corpo de um símbolo por span de AST. | `Edit` + leitura manual |
| `brain_write` | Criar ou substituir um arquivo inteiro. | `Write` |
| `brain_write_many` | Gravar vários arquivos como um lote atômico. | várias chamadas de `Write` |
| `brain_patch` | Aplicar um diff unificado com verificação prévia. | `git apply`, `patch` |
| `brain_exec` | Rodar `cargo build/check/test/clippy` e agrupar erros. | `cargo build`, `cargo test` |
| `brain_shell` | Rodar git, formatador e ferramenta externa. | `shell` |
| `brain_peek` | Paginar uma resposta truncada. | — |
| `brain_ledger` | Registrar e consultar decisões duráveis do projeto. | notas soltas |
| `brain_fetch` | Baixar uma URL e devolver só os blocos relevantes. | `WebFetch` |
| `brain_websearch` | Buscar na web sem provedor pago. | `WebSearch` |
| `brain_research` | Buscar, ler fontes e devolver evidência numa chamada. | busca + fetch manual |

---

### Busca e leitura

#### `brain_search`

Fonte de verdade do código. Combina busca lexical com AST e devolve o corpo completo do símbolo que casou, com `path:start-end`.

Parâmetros:

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `natural_query` | string | obrigatório | Descrição em linguagem natural do que se procura. |
| `keywords` | string[] | obrigatório | Termos técnicos, nomes ou regex curtas para ancorar a busca. |
| `detail` | `auto`, `full` ou `signatures` | `auto` | `auto` entrega o corpo dos primeiros e a assinatura do resto. |
| `max_results` | inteiro | `5` | Número máximo de recortes. |
| `min_confidence` | número | `0.65` | Aceito por compatibilidade; sem corte no modo lexical. |
| `repeat` | `allow` | — | Força a reentrega de corpos já vistos. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

Exemplo:

~~~json
{
  "natural_query": "onde o token JWT é validado",
  "keywords": ["verify_token", "jwt", "decode"],
  "detail": "auto",
  "max_results": 5
}
~~~

Use `detail: "signatures"` quando só precisa saber onde está, sem pagar pelo corpo.

#### `brain_read`

Lê o **esqueleto** do arquivo (assinaturas) ou o **corpo** de um símbolo.

| Nome | Tipo | Descrição |
|---|---|---|
| `path` | string | Caminho relativo ao root. |
| `rev` | string | Lê `rev:path` (commit, tag ou ref, como `HEAD~1`) em vez da árvore de trabalho. Somente leitura. |
| `symbol` | string | Nome simples (`verify_token`) ou qualificado (`AuthService::verify_token`). |
| `repeat` | `allow` | Força a reentrega do corpo. |
| `budget_tokens` | inteiro | Teto de tokens. |

Sem `symbol`, devolve o esqueleto do arquivo. Com `symbol`, devolve só o corpo e o span exato.

Com `rev`, o conteúdo vem do repositório e não do worktree: serve para arquivo deletado ou versão antiga, e o hash devolvido não vale para `brain_edit`.

#### `brain_map`

Esqueleto do projeto, ordenado pela centralidade (quantas chamadas o símbolo recebe).

| Nome | Tipo | Descrição |
|---|---|---|
| `path` | string | Restringe o mapa a um prefixo de caminho, como `src/auth`. |
| `budget_tokens` | inteiro | Teto de tokens. |

Use quando você não sabe onde está. Para achar um símbolo específico, prefira `brain_search`.

#### `brain_context_graph`

Fonte de verdade das relações. Mostra quem chama e o que o símbolo ou arquivo consome, com assinaturas e linhas de chamada.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `target_symbol_or_file` | string | obrigatório | Nome do símbolo ou caminho relativo do arquivo. |
| `depth` | inteiro | `1` | Saltos no grafo (1 a 3). |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

#### `brain_impact_check`

Fonte de verdade do impacto. Lista os chamadores reversos que quebram se a assinatura mudar.

| Nome | Tipo | Descrição |
|---|---|---|
| `include_tests` | boolean | Inclui chamadores de teste; por padrão são contados e omitidos. |
| `symbol_name` | string | Nome da função, método ou struct que será modificada. |
| `budget_tokens` | inteiro | Teto de tokens. |

Rode antes de mudar uma assinatura pública.

#### `brain_assert`

Responde uma pergunta categórica, sem despejar código:

| `kind` | Pergunta |
|---|---|
| `exists` | O símbolo existe no índice? |
| `public` | O símbolo é visível fora do arquivo? |
| `used_outside` | Há chamadas de outro arquivo ou módulo? |

| Nome | Tipo | Descrição |
|---|---|---|
| `kind` | `exists`, `public` ou `used_outside` | Pergunta. |
| `symbol` | string | Nome exato do símbolo. |
| `scope` | string | Para `used_outside`, prefixo considerado "dentro". |
| `budget_tokens` | inteiro | Teto de tokens. |

#### `brain_diff`

Analisa o diff atual: símbolos alterados, chamadores externos e testes afetados. Responde "o que quebrei?" sem ler o diff inteiro.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `base` | string | `HEAD` | Revisão base do git. |
| `include` | array | ambas | `impact`, `tests` ou ambos. |
| `path` | string | — | Restringe a um prefixo de caminho. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

#### `brain_trace`

Caminho de chamadas entre dois símbolos (A → … → B) em uma resposta curta.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `from` | string | obrigatório | Símbolo ou arquivo de origem. |
| `to` | string | obrigatório | Símbolo ou arquivo de destino. |
| `max_depth` | inteiro | `6` | Máximo de saltos (1 a 12). |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

#### `brain_why`

Histórico git de um símbolo: commits que tocaram suas linhas, com autor, data e assunto.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `symbol` | string | obrigatório | Nome exato da função ou struct. |
| `max_commits` | inteiro | `10` | Máximo de commits (1 a 50). |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

---

### Escrita

#### `brain_edit`

Edita o **corpo** de um símbolo por span de AST. Não reformata, não reindenta e não toca no resto do arquivo.

| Nome | Tipo | Descrição |
|---|---|---|
| `path` | string | Caminho relativo ao root. |
| `symbol` | string | Nome simples ou qualificado. |
| `expected_hash` | string | blake3 do arquivo, como o `brain_read` imprimiu. |
| `new_body` | string | Bloco `{ … }` completo, com chaves. |
| `signature` | string | Opcional: substitui a assinatura antes do `{`. |
| `budget_tokens` | inteiro | Teto de tokens da resposta. |

Recusa hash divergente, símbolo ambíguo e sintaxe inválida. Em caso de retry após resposta perdida, informa que a edição já foi aplicada e não reescreve.

#### `brain_write`

Cria ou substitui um arquivo inteiro (configs, docs, módulos). Escrita atômica e relida.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `path` | string | obrigatório | Caminho relativo ao root. |
| `content` | string | obrigatório | Conteúdo completo em UTF-8. |
| `expected_hash` | string | — | Obrigatório para sobrescrever um arquivo existente. |
| `overwrite` | boolean | `false` | Substitui sem conferir hash. Use com intenção. |
| `create_only` | boolean | `false` | Falha se o arquivo já existir. |
| `create_parents` | boolean | `true` | Cria diretórios intermediários. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

#### `brain_patch`

Aplica um diff unificado no formato `git diff`, com `git apply --check` antes de escrever.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `patch` | string | obrigatório | Diff completo, incluindo `diff --git`, `---`, `+++` e hunks. |
| `check_only` | boolean | `false` | Só verifica se aplica; não escreve. |
| `reverse` | boolean | `false` | Aplica ao contrário, para desfazer. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

Cobre arquivos novos e não-Rust. Recusa caminhos fora do root.

---

### Execução

#### `brain_exec`

Fonte de verdade do build e dos testes. Roda o Cargo e devolve erros agrupados, falhas com o assert intacto e cache.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `kind` | `build`, `check`, `test` ou `clippy` | obrigatório | `check` é o mais rápido para erros de tipo. |
| `refresh` | boolean | `false` | Ignora o cache e reexecuta. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

O resultado é cacheado por hash das fontes. Se nada mudou, a segunda chamada é servida do cache. O timeout é `--exec-timeout-secs` (padrão 300 s).

#### `brain_shell`

Executa um comando em `bash -lc` no root. É para **git, formatador e ferramenta externa**, não para leitura de código.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `command` | string | obrigatório | Comando completo. |
| `timeout_secs` | inteiro | `--shell-timeout-secs` | Tempo máximo. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

A guarda é descrita na seção seguinte.

---

### Sessão

#### `brain_peek`

Recupera o trecho de uma resposta truncada.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `handle` | string | obrigatório | Handle `h_xxxxxxxx` devolvido no envelope. |
| `offset_chars` | inteiro | `0` | Posição em caracteres no corpo omitido. |
| `budget_tokens` | inteiro | `2000` | Tamanho da página. |

#### `brain_ledger`

Registra e consulta fatos e decisões duráveis do projeto em `.brain/ledger.jsonl`. Append-only e deduplicado por conteúdo.

| Nome | Tipo | Descrição |
|---|---|---|
| `action` | `record`, `list` ou `search` | Ação. |
| `text` | string | Fato a gravar (`record`) ou termos da busca (`search`). |
| `tags` | string[] | Rótulos opcionais. |
| `budget_tokens` | inteiro | Teto de tokens. |

Use para não redescobrir a mesma decisão depois.

---

### Web

#### `brain_fetch`

Baixa uma URL e devolve só os blocos relevantes, verbatim, com âncora `#blk_xxxxxxxx`. O excedente fica em `brain_peek`.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `url` | string | obrigatório | URL http(s) absoluta. |
| `question` | string | — | O que você quer saber; ranqueia os blocos. |
| `max_blocks` | inteiro | `80` | Máximo de blocos considerados antes do orçamento. |
| `refresh` | boolean | `false` | Ignora o cache local e revalida com o servidor. |
| `budget_tokens` | inteiro | `2000` | Teto de tokens. |

Não releia a mesma URL para conferir: a resposta diz quando o conteúdo foi revalidado e reutiliza o cache.

#### `brain_websearch`

Busca na web sem provedor pago. Consulta fontes gratuitas, funde rankings (RRF) e remove duplicatas.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `query` | string | obrigatório | Consulta em linguagem natural. |
| `max_results` | inteiro | `8` | Resultados após fusão e dedupe (1 a 20). |
| `locale` | string | — | Região preferida, como `pt-BR` ou `en-US`. |
| `budget_tokens` | inteiro | `900` | Teto de tokens. |

Devolve título, URL, domínio e trecho. Para ler o conteúdo, use `brain_fetch` com `question`.

#### `brain_research`

Busca, lê as melhores fontes e devolve a evidência numa chamada. Funde provedores, baixa os top resultados em paralelo e entrega só os blocos que respondem à pergunta, com URL e âncora para citar.

| Nome | Tipo | Padrão | Descrição |
|---|---|---|---|
| `query` | string | obrigatório | Consulta de busca. |
| `question` | string | — | O que você quer saber; ranqueia os blocos de cada fonte. |
| `max_sources` | inteiro | `3` | Quantas fontes baixar e ler (1 a 6). |
| `locale` | string | — | Região preferida. |
| `budget_tokens` | inteiro | `1200` | Teto de tokens. |

Use quando a pergunta precisa de fontes externas. Para uma URL já conhecida, use `brain_fetch`.

---

## Guarda do shell

O `brain_shell` é o único caminho para processos. A guarda vem **ligada por padrão** e recusa comandos que outra ferramenta já cobre.

### O que é recusado

| Comando | Substituto |
|---|---|
| `grep`, `egrep`, `fgrep`, `rg`, `ag`, `ack`, `ugrep` | `brain_search` |
| `cat`, `bat`, `less`, `more`, `most`, `head`, `tail`, `nl`, `tac`, `strings`, `xxd`, `od`, `hexdump` | `brain_read` |
| `awk`, `gawk`, `mawk`, `cut` | `brain_read` |
| `sed` (sem `-i`) | `brain_read` |
| `sed -i` | `brain_edit` |
| `wc` com arquivo | `brain_read` |
| `find`, `fd`, `fdfind`, `locate`, `tree`, `ls`, `dir`, `vdir` | `brain_map` |
| `tee`, `dd`, `truncate` | `brain_write` |
| `patch` | `brain_patch` |
| `git grep` | `brain_search` |
| `git diff`, `git show` | `brain_diff` |
| `git cat-file <rev>:<path>`, `git show <rev>:<path>` | `brain_read` com `rev` |
| `git blame`, `git log -L` | `brain_why` |
| `git apply` | `brain_patch` |
| `git ls-files`, `git ls-tree` | `brain_map` |
| `cargo build`, `cargo check`, `cargo test`, `cargo clippy` | `brain_exec` |
| `python3 -c`, `node -e`, `perl -e`, `ruby -e`, `php -r` (código inline) | `brain_read` ou `brain_search` |
| `python3 - <<EOF`, `bash <<EOF` (código por heredoc) | `brain_edit` |

Além dos programas, a guarda recusa **redirecionamentos para arquivo** (exceto `/dev/null` e duplicação de descritor). Um `> arquivo` é tratado como escrita e aponta para `brain_write`.

### O que passa

- `git status`, `git log`, `git add`, `git commit` e outros subcomandos não listados.
- `cargo fmt`, `cargo metadata`, `cargo tree`.
- Ferramentas externas não cobertas, como `rustc`, `jq`, `curl`, `openssl`.
- Scripts a partir de arquivo (`python3 tools/build.py`), que continuam sendo ferramenta externa.

### Como a guarda analisa

- Divide pipelines e verifica **cada segmento**.
- Desembrulha wrappers e shells aninhados (`bash -c`, `env`, `sudo`, etc.).
- Lê subcomandos de `git` e `cargo` depois das opções globais.
- Recusa código inline em interpretador pela **forma**, não pelo conteúdo: `python3 -c`, `node -e`, `perl -r` e heredoc (`python3 - <<EOF`). Nenhuma lista de pistas sobrevive a ofuscação, então a regra é categórica.
- `wc` sem arquivo é permitido, porque é só um consumidor de pipe.

### Desligar

~~~sh
brain-tools --shell-guard false
# ou
BRAIN_SHELL_GUARD=false brain-tools
~~~

Desligar a guarda não é recomendado em uso normal. A guarda é uma regra de comportamento, **não um sandbox**: ela não impede `rm`, não inspeciona scripts executados a partir de arquivo nem código passado de forma indireta (base64 para `sh`, `eval` construído em runtime), e não substitui permissões do host.

### Log de uso

Por padrão, o `brain_shell` grava metadados em `.brain/shell-usage.jsonl`:

- timestamp, pid, cwd, programa, status, código de saída, duração, timeout;
- bytes de stdout/stderr;
- comando **redigido** (segredos são mascarados).

Com `--shell-usage-log-full`, o comando cru e os primeiros 200 caracteres de stdout/stderr também são gravados. Isso é opt-in porque pode capturar segredos.

Para desligar:

~~~sh
brain-tools --shell-usage-log false
~~~

---

## Bloqueio das ferramentas alternativas

A guarda do `brain_shell` cobre o que passa **pelo** `brain_shell`. Para o agente não usar as ferramentas nativas do host (Read, Edit, Grep, Bash, WebFetch, etc.), a barreira precisa estar nas permissões do cliente.

### Modelo de permissões do OpenCode V2

Cada regra tem três campos:

~~~json
{ "action": "shell", "resource": "*", "effect": "deny" }
~~~

- `action`: a ação (ferramenta ou categoria).
- `resource`: o valor usado (caminho, comando, URL, consulta ou ID).
- `effect`: `allow`, `deny` ou `ask`.

Regras são avaliadas **em ordem** e a **última que casa vence**. Se nenhuma casar, o padrão é `ask`. Quando uma operação toca vários recursos, qualquer `deny` nega.

Ações relevantes:

| Ação | Bloqueia |
|---|---|
| `read` | Leitura nativa de arquivo. |
| `edit` | `edit`, `write` e `patch` nativos. |
| `glob` | Busca por padrão de nome. |
| `grep` | Busca por regex. |
| `shell` | Comandos de shell. |
| `subagent` | Criação de subagente. |
| `webfetch` | Fetch nativo de URL. |
| `websearch` | Busca nativa na web. |
| `external_directory` | Acesso fora do projeto. |
| `execute` | Code Mode e as ferramentas alcançadas por ele. |
| `brain-tools_*` | Ferramentas do MCP `brain-tools`. |

### Configuração estrita (recomendada)

O `opencode.json` deste projeto define o agente `brain-only` com uma **allowlist**: tudo é negado por padrão e só o que está listado é liberado.

~~~json
{
  "$schema": "https://opencode.ai/config.json",
  "default_agent": "brain-only",
  "mcp": {
    "servers": {
      "brain-tools": {
        "type": "local",
        "command": [
          "/caminho/absoluto/para/brain_tools/target/release/brain-tools",
          "--root-dir",
          "/caminho/absoluto/para/o/projeto"
        ],
        "environment": { "RUST_LOG": "info" }
      }
    }
  },
  "agents": {
    "brain-only": {
      "description": "Usa o MCP brain-tools para toda operação de código.",
      "mode": "primary",
      "permissions": [
        { "action": "*", "resource": "*", "effect": "deny" },
        { "action": "execute", "resource": "*", "effect": "allow" },
        { "action": "brain-tools_*", "resource": "*", "effect": "allow" },
        { "action": "opencode_list_mcp_resources", "resource": "brain-tools", "effect": "allow" },
        { "action": "opencode_read_mcp_resource", "resource": "brain-tools:*", "effect": "allow" },
        { "action": "question", "resource": "*", "effect": "allow" }
      ]
    }
  }
}
~~~

Por que essa ordem funciona:

1. `*` + `deny` fecha **tudo**, inclusive ferramentas nativas e ferramentas futuras.
2. `execute` + `allow` reabre o Code Mode, que é como o agente chama o MCP.
3. `brain-tools_*` + `allow` reabre somente as ferramentas do servidor.
4. `question` fica liberado para o agente pedir esclarecimento.
5. As regras do agente são acrescentadas **depois** da política global; por isso a allowlist vence o `allow` padrão.

Sem `execute`, o Code Mode desaparece e as ferramentas alcançadas só por ele deixam de funcionar. Sem `brain-tools_*`, o agente não tem como ler o projeto.

### Variante com denylist

Se você quiser manter as ferramentas nativas para outros agentes e restringir só o `brain-only`, troque a allowlist por negações explícitas:

~~~json
{
  "agents": {
    "brain-only": {
      "description": "Somente brain-tools.",
      "mode": "primary",
      "permissions": [
        { "action": "read", "resource": "*", "effect": "deny" },
        { "action": "edit", "resource": "*", "effect": "deny" },
        { "action": "glob", "resource": "*", "effect": "deny" },
        { "action": "grep", "resource": "*", "effect": "deny" },
        { "action": "shell", "resource": "*", "effect": "deny" },
        { "action": "subagent", "resource": "*", "effect": "deny" },
        { "action": "webfetch", "resource": "*", "effect": "deny" },
        { "action": "websearch", "resource": "*", "effect": "deny" },
        { "action": "external_directory", "resource": "*", "effect": "deny" },
        { "action": "execute", "resource": "*", "effect": "allow" },
        { "action": "brain-tools_*", "resource": "*", "effect": "allow" }
      ]
    }
  }
}
~~~

A allowlist é preferível porque uma ferramenta nova no host nasce **negada**; a denylist nasce liberada até alguém lembrar de negá-la.

### Limites dessa barreira

- Permissões são por **ferramenta**, não um sandbox de sistema operacional.
- O Code Mode (`execute`) expõe `fetch` para HTTP. Negar `webfetch` não bloqueia rede via `execute`; para uma barreira dura, use `policies`.
- O agente ainda pode executar qualquer coisa que o MCP permitir. Se quiser menos superfície, use `--tools read` e desligue exec/shell no servidor.
- A configuração do projeto pode ser sobreposta por configuração global e por policies; revise as duas.

### Outros clientes

Clientes sem um modelo de permissões como o do OpenCode não conseguem bloquear as ferramentas nativas por configuração. Nesse caso, reduza a superfície no próprio servidor:

~~~sh
brain-tools --tools read --exec-disabled --shell-disabled
~~~

E mantenha um `AGENTS.md` com a regra de comportamento. Exemplo mínimo:

~~~md
# Regras do projeto

Use sempre o MCP brain-tools para ler, buscar e editar código.

- Busca: brain_search
- Leitura: brain_read
- Estrutura: brain_map
- Relações: brain_context_graph e brain_impact_check
- Edição: brain_edit, brain_write e brain_patch
- Build/testes: brain_exec
- Git e formatadores: brain_shell

Não use grep, cat, find, sed, cargo ou git diff diretamente.
~~~

O `AGENTS.md` é uma instrução; a permissão é a barreira. Use os dois.

---

## Fluxo de trabalho recomendado

1. **Orientar-se**: `brain_map` quando não sabe onde está.
2. **Encontrar**: `brain_search` com termos e nomes de função.
3. **Entender relações**: `brain_context_graph` ou `brain_impact_check`.
4. **Ler**: `brain_read` no símbolo, usando o hash do cabeçalho.
5. **Editar**: `brain_edit` com `expected_hash`; para arquivo inteiro, `brain_write`; para diff, `brain_patch`.
6. **Verificar**: `brain_exec` com `check` ou `test`.
7. **Revisar o impacto**: `brain_diff` antes de encerrar.
8. **Entender o passado**: `brain_why` quando o código parece estranho.

Se uma chamada MCP falhar, diga isso explicitamente. Não caia em `grep` em silêncio: a degradação precisa ser visível.

### Exemplo de ciclo

~~~text
brain_search   → encontra verify_token em src/auth.rs:120-180
brain_read     → corpo + hash do arquivo
brain_edit     → novo corpo, com expected_hash
brain_exec     → check
brain_diff     → chamadores externos e testes afetados
~~~

---

## Desenvolvimento

~~~sh
cargo build --release
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
~~~

O perfil de release usa LTO completo e um único codegen unit. O build é lento de propósito; para o ciclo de desenvolvimento, use `cargo check` ou `cargo test`.

Estrutura de testes:

- `crates/brain-tools/tests/` — integração do servidor, protocolo, configuração, watcher, edição e resposta.
- `crates/brain-graph/tests/` — resolução de escopo entre arquivos e crates.
- `crates/brain-exec/tests/` — execução, parser de erros e cache.
- `crates/brain-web/tests/` — extração, dedupe, fusão e pesquisa.
- `crates/brain-kernel/tests/` — cache de parse.

Benchmarks:

~~~sh
cargo run --release -p brain-bench -- --help
~~~

---

## Solução de problemas

### O servidor não conecta

- Confirme o caminho **absoluto** do binário em `opencode.json`.
- Rode o binário manualmente e veja o stderr:
  ~~~sh
  RUST_LOG=debug /caminho/para/brain-tools --root-dir /caminho/do/projeto
  ~~~
- O servidor fala JSON-RPC em stdout; logs vão para stderr. Não redirecione stderr para stdout.
- No OpenCode, use `/mcps` ou `opencode mcp list` para ver o status.

### A ferramenta não aparece

- Verifique o perfil (`--tools`). `core` e `read` não expõem todas as ferramentas.
- Verifique `--exec-disabled` e `--shell-disabled`.
- O perfil `read` não tem `brain_edit`, `brain_write`, `brain_patch` nem `brain_exec`.

### `brain_edit` recusa o hash

O arquivo mudou desde o `brain_read`. Leia de novo e use o hash atual. Não use `overwrite` para contornar; isso reintroduz o risco de escrita obsoleta.

### A resposta veio truncada

Use o handle com `brain_peek` e pagine com `offset_chars`. Se o handle expirou, refaça a chamada com um `budget_tokens` maior ou `repeat: "allow"`.

### O corpo não veio de novo

É o ledger de entregas. Use `repeat: "allow"` quando precisar do texto novamente.

### `brain_shell` recusa um comando

A guarda está ligada. Use a ferramenta indicada na mensagem. Se for um falso positivo, verifique se o comando realmente não é coberto; desligar a guarda inteira deve ser a última opção.

### `brain_exec` estoura o tempo

Aumente `--exec-timeout-secs` ou rode um alvo menor. O timeout devolve o que foi capturado.

---

## Segurança

- `brain_shell` **não é um sandbox**: roda com o usuário, o sistema de arquivos, os processos e a rede do host.
- A guarda é de comportamento, não de isolamento. Não contorne por script, interpretador ou alias.
- `brain_exec` executa `cargo` e, portanto, build scripts e testes do projeto. Trate um repositório não confiável como código executável.
- `brain_fetch`, `brain_websearch` e `brain_research` acessam a rede. Não envie segredos em URLs ou consultas.
- O log do shell redige segredos por padrão; `--shell-usage-log-full` pode capturar conteúdo sensível.
- Permissões do OpenCode são por ferramenta. Para uma barreira organizacional, use `policies` e configuração global gerenciada.
