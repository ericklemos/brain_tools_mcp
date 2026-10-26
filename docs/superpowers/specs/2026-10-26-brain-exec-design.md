# `brain_exec` — Execução de Build e Testes com Resumo Determinístico

**Data:** 2026-10-26
**Status:** Proposto — aguardando revisão
**Sub-projeto:** B de A–E (execução)

## Contexto

As três ferramentas atuais cobrem **encontrar** e **entender** código. O ciclo de um
agente continua depois disso: compilar, testar, corrigir. Em todas essas etapas ele volta
ao regime bruto — `cargo build` despeja dezenas a centenas de KB de log no contexto, e o
agente relê a maior parte disso para extrair 3 erros.

Isso é o vazamento dominante do ciclo depois da busca: economizar 2k tokens numa busca
cirúrgica e gastar 40k num build não fecha a conta.

Duas propriedades tornam esse vazamento evitável:

1. **`cargo` já emite diagnóstico estruturado.** `--message-format=json` produz um
   `Diagnostic` por erro, com `level`, `message`, `code`, `spans` e `children`. Não há
   nada a inferir do texto.
2. **A inferência custaria o que se quer evitar.** Um subagente lendo o log para resumir
   paga o log inteiro no contexto *de alguém*, e devolve um resumo não-determinístico
   que muda entre execuções. O parser determinístico custa zero token de LLM.

### Dependência

Este sub-projeto assume o contrato de retorno do **sub-projeto A**
(`docs/superpowers/specs/2026-10-26-response-contract-design.md`): `Section`,
orçamento por `budget_tokens`, envelope de truncamento e `brain_peek`.

Se A ainda não estiver implementado quando B começar, a ordem inverte: implementar A
primeiro. A alternativa — `brain_exec` inventar seu próprio truncamento — é exatamente o
que A existe para impedir, e produziria dois formatos de envelope no mesmo servidor.
O desenho abaixo não repete código de orçamento: só escolhe prioridades de `Section`.

## Objetivo

Uma ferramenta `brain_exec` que roda as quatro operações canônicas de build em Rust e
devolve um relatório agrupado, dentro do orçamento de tokens, recuperável por handle.

## Não-objetivos

- **Kotlin/Gradle na v1.** Gradle não tem equivalente a `--message-format=json`; cobri-lo
  exige um parser heurístico de log, que é essencialmente um segundo sub-projeto.
- **Nextest na v1.** Ver Decisões.
- Comando livre. Nenhum caminho aceita uma string de shell.
- Parsing semântico de sugestões (`children`/`suggestion`) — o agente reescreve a
  correção melhor que o `rustc` sugere, e é o maior peso da resposta.
- Cache em disco ou entre processos.
- Edição de arquivos. `brain_exec` lê e compila; nunca escreve fonte.

## Desenho

### 1. Contrato da ferramenta

Uma tool, não três. Cada tool registrada custa schema e descrição em **todo** request,
inclusive nas conversas que não compilam nada.

```json
{
  "name": "brain_exec",
  "input": {
    "kind":          "build | check | test | clippy   (obrigatório)",
    "budget_tokens": "integer, opcional — herda o default de A",
    "refresh":       "boolean, default false"
  }
}
```

| `kind` | Invocação |
|---|---|
| `build` | `cargo build --message-format=json --color=never` |
| `check` | `cargo check --message-format=json --color=never` |
| `test` | `cargo test --no-fail-fast --color=never` |
| `clippy` | `cargo clippy --message-format=json --color=never` |

- `--manifest-path` é derivado de `root_dir/Cargo.toml`. O modelo escolhe *qual operação*,
  nunca *o que executar*; a superfície fica fechada em quatro valores de enum.
- `clippy` **não** usa `-D warnings`. Transformar warning em erro silenciosamente mudaria
  o significado de um build verde.
- `--no-fail-fast` no `test`: parar no primeiro binário de teste esconde o resto das
  falhas, e o agente faria N chamadas para descobrir N falhas.
- Timeout default **300 s** (`--exec-timeout-secs` / `BRAIN_EXEC_TIMEOUT_SECS`). Ao
  estourar: mata o processo e devolve o que já foi capturado, com aviso explícito no topo.
  Nunca devolve resposta vazia.
- Um `tokio::Mutex` serializa as execuções no servidor. O `cargo` já trava o `target/`;
  duas chamadas concorrentes ficariam penduradas sem explicação. A segunda espera e
  normalmente recebe o resultado do cache da primeira.
- `exec_enabled` (default `true`, `--exec-disabled` / `BRAIN_EXEC_DISABLED`) remove a tool
  do registry no boot, para ambientes onde execução de processo não é aceitável.

**Rede de segurança:** se o parser extrair zero diagnósticos e o exit code for ≠ 0, a
resposta é a cauda crua do stderr (últimas 40 linhas), rotulada `Saída bruta (parser não
reconheceu o erro)`. Nunca `0 erros` num build que falhou. Cobre linker, build script e
qualquer coisa que o `cargo` reporte fora do JSON.

### 2. Parser de diagnóstico (`build` / `check` / `clippy`)

Lê o stdout linha a linha como JSON e considera só `reason: "compiler-message"` e
`reason: "build-finished"`.

- **Agrupamento por `(code, arquivo)`.** O mesmo `E0308` em 12 call-sites vira uma entrada
  com `12 ocorrências:` e a lista `linha → símbolo`. É o que o agente precisa para
  decidir se é um erro ou doze.
- **Erros em expansão de macro** (`span.expansion`) são atribuídos ao call-site e
  marcados `(em macro)`. Sem isso o mesmo erro aparece N vezes com spans quase idênticos.
- **Renderização por entrada:** `arquivo:linha` · nível · código · primeira linha da
  `message` · a linha-fonte com o `^` do span primário. O JSON do `cargo` traz o span
  (`line_start`, `column_start`, `column_end`) mas **não** o texto da linha; o parser lê
  essa linha do arquivo em disco. Falha de leitura apenas omite a linha-fonte — nunca
  descarta o diagnóstico.
- **Warnings:** apenas a contagem por código e os 3 códigos mais frequentes, cada um com
  um `arquivo:linha` de exemplo. O corpo completo vai para o handle.

### 3. Parser de teste (`test`)

`cargo test` não tem saída JSON em stable; o libtest emite texto delimitado. O parser
fatia por `---- <nome> stdout ----`, que é estável há anos.

- Preserva o bloco de `panicked at` **inteiro** (é onde vivem `left`/`right`) e as últimas
  ~20 linhas da seção.
- O miolo vira `[… N linhas omitidas — brain_peek h_xxxx]`.
- O resumo `N falharam, M passaram` é agregado entre todos os binários de teste: o
  `cargo test` roda vários, e o total tem que ser um só.
- Uma `Section` por teste que falha.

### 4. Prioridades no orçamento

| Seção | Prioridade |
|---|---|
| Falha de teste / erro de compilação | 255 |
| Resumo (`N erros em M arquivos`) | 200 |
| Saída bruta de fallback / timeout | 255 |
| Warnings | 100 |

### 5. Cache

Chave = `blake3(kind ‖ hashes_dos_fontes ‖ estado_dos_manifests)`. Uma entrada em memória
por `kind` (quatro no total). Sem LRU, sem TTL, sem disco.

- Mesma chave do último run **verde** → `build ok (cache, 2.1s)`. Uma linha.
- Mesma chave do último run **vermelho** → o mesmo relatório, marcado `(cache)`.
- `refresh: true` ignora e recomputa.

**Fontes vs. manifests.** O watcher só indexa `.rs`/`.kt`/`.kts`
(`crates/brain-graph/src/watcher.rs:28`); `Cargo.toml` e `Cargo.lock` **não** entram em
`file_hashes`. Por isso a chave usa duas fontes distintas:

1. `ProjectCodeGraph::file_hashes` — os `.rs`, ordenados por caminho.
2. `mtime` + tamanho de `Cargo.toml` e `Cargo.lock`.

**Correção do cache contra o watcher assíncrono.** O watcher tem debounce de 50 ms
(`watcher.rs:48`). Um arquivo salvo há 5 ms pode não estar no grafo, e o cache devolveria
o resultado de antes da edição — o pior erro possível, porque o agente acredita nele.
Antes de servir um hit, o conteúdo em disco de cada arquivo indexado é re-hasheado e
comparado com `file_hashes`. Divergência força recomputação. É leitura de arquivos já em
cache de página, barato frente ao custo de um build errado.

### 6. Configuração

Novas entradas em `crates/brain-kernel/src/config.rs`, ao lado das existentes:

| Flag | Env | Default |
|---|---|---|
| `--exec-timeout-secs` | `BRAIN_EXEC_TIMEOUT_SECS` | `300` |
| `--exec-disabled` | `BRAIN_EXEC_DISABLED` | `false` |

## Arquivos

**Novos:** `crates/brain-exec/src/{lib,exec_tool,runner,parse_cargo,parse_libtest,cache,schema}.rs`
e `crates/brain-exec/Cargo.toml`.

**Alterados:**
- `Cargo.toml` (workspace) → membro novo e dependência compartilhada.
- `crates/brain-tools/src/main.rs` → registra `ExecTool` (uma chamada, respeitando
  `exec_enabled`).
- `crates/brain-kernel/src/config.rs` → as duas flags acima.
- `crates/brain-tools/tests/brain_search_integration_test.rs:68` e
  `graph_tools_test.rs:72` → `tools.len()` de `3` (ou `4`, se A já entrou) para incluir
  `brain_exec`.

## Testes

- **Fixtures capturados de execuções reais**, não escritos à mão: um `cargo check --message-format=json`
  com erro, um com warnings, e uma saída de `cargo test` com falhas. O formato do `cargo`
  é a fonte da verdade; fixture inventado testa o fixture.
- Agrupamento: 12 ocorrências do mesmo `E0308` produzem uma entrada com 12 linhas.
- Macro: erro dentro de `span.expansion` aparece uma vez por call-site, marcado `(em macro)`.
- Teste: bloco de `panicked at` presente inteiro; miolo ausente e marcado como omitido.
- Teste: múltiplos binários agregam num único resumo.
- Cache: hit verde devolve uma linha; hit vermelho devolve o relatório; `refresh` ignora;
  hash divergente em disco invalida o hit.
- Timeout: processo longo é morto e a resposta traz o parcial mais o aviso.
- Fallback: exit ≠ 0 com zero diagnósticos devolve a cauda do stderr, nunca "sem erros".

## Decisões e riscos

- **Libtest-text em vez de nextest.** O texto do libtest está sempre disponível e se
  fatia por delimitador estável; o nextest não está instalado em todo lugar. Detectar
  nextest quando existir faria o parser mudar conforme a máquina, e os testes de
  integração passariam a depender do ambiente. Revisitar se o uso mostrar que a saída do
  libtest é insuficiente.
- **Kotlin fica de fora da v1.** Deliberado: o parser de log do Gradle é heurístico e
  merece spec próprio, com suas próprias fixtures.
- **Execução de testes roda código do repositório.** É o único risco da tool, e é o mesmo
  de um `cargo test` que o agente já rodaria por conta própria. `exec_enabled` permite
  desligar a tool inteira onde isso não for aceitável.
- **Cache em memória por `kind` pode servir resultado obsoleto em edições fora do
  watcher** (arquivo criado por script, checkout de git). A re-verificação por hash no
  hit cobre o conteúdo dos `.rs` já indexados, mas não arquivos novos que o grafo ainda
  não conhece nem `.rs` em `target/`. Mitigação em duas partes: `refresh: true` sempre
  disponível, e o tapete de segurança da Seção 1 — como todo desfecho de não-sucesso
  carrega resumo ou saída bruta, um cache obsoleto no pior caso devolve o relatório de
  erro antigo, nunca um "ok" falso.
- **`cargo check` e `cargo build` produzem diagnósticos equivalentes** para a maioria dos
  erros. A tool não tenta escolher por conta própria qual é suficiente; o agente pede.
- **Timeout de 300 s pode ser curto** em build limpo de workspace grande. Configurável, e
  o estouro devolve o parcial em vez de nada.

## Critério de sucesso

1. Um build quebrado com 40 erros em 12 arquivos cabe no orçamento e ainda identifica
   todos os arquivos afetados.
2. Uma falha de teste chega ao agente com o bloco de assert intacto.
3. O agente nunca recebe "0 erros" quando o exit code indicou falha.
4. Chamar de novo sem alterar nada não reexecuta o `cargo`.
5. Nenhum caminho da tool aceita string de comando arbitrária.
