# `brain-bench` — medindo a economia de tokens

> Sem número, "economia de tokens" é opinião. O `brain-bench` responde uma
> pergunta só: **quanto custa, em tokens, responder uma pergunta fixa com o
> `brain-tools`, comparado com reler os arquivos que a resposta cobre?**

## O que é medido

O harness sobe o binário real por stdio, faz `initialize` e `tools/list`, e
replaya uma lista fixa de tarefas em `bench/tasks.json`. Para cada chamada ele
conta:

- `request_tokens` — os argumentos JSON que o agente envia;
- `response_tokens` — o texto que o agente recebe.

O total (`brain`) é a soma dos dois: é o que a chamada realmente ocupa de
contexto. O contrafactual (`naïve`) é o tamanho, verbatim do disco, dos
arquivos que a tarefa declara — o que um agente sem o servidor abriria com
`Read`.

Os tokens são estimados com `brain_kernel::budget::estimate_tokens`
(`chars / 3.5`), a mesma função que o servidor usa para decidir o que cabe no
budget. "Tokens" aqui e "budget tokens" ali são a mesma unidade.

O relatório também mede o `tools/list` separado, porque ele é pago **por
requisição** em vários clientes, não por pergunta.

## Regras que mantêm o número honesto

1. **O baseline é declarado, nunca inferido.** A lista `naive` de cada tarefa
   é escrita à mão. Inferir dos caminhos que o servidor citou deixaria o
   servidor corrigir a própria prova.
2. **Tarefa sem baseline é rejeitada.** Razão contra zero não é medição.
3. **Entrada `naive` inexistente é erro**, não zero silencioso: um typo
   inflaria todas as economias do relatório.
4. **Ferramentas com estado são rejeitadas.** `brain_exec`, `brain_peek` e
   `brain_ledger` dependem de um build anterior, de um handle vivo ou de
   entradas passadas; o baseline se moveria sozinho.
5. **`--update` recusa gravar com tarefa falhando.** Congelar uma falha como
   referência faria todas as execuções seguintes parecerem limpas.

`brain_diff` não está no conjunto de tarefas por um motivo diferente: a saída
dele depende do estado do repositório (árvore de trabalho, histórico), não do
custo de responder. Se for medir, fixe `base` num commit concreto.

## Como rodar

```bash
cargo build --release                 # o bench mede o binário real
cargo run -p brain-bench -- --update  # grava bench/baseline.json
cargo run -p brain-bench              # compara com o baseline e falha se crescer
```

Sem `target/release/brain-tools`, o harness cai para `target/debug/brain-tools`
e avisa. O `--root-dir` do servidor é o `--root` do bench (padrão `.`).

Saída típica:

```
tarefa                       brain     naïve    fator    tempo  resultado
busca-orcamento              1.185     7.397     6,2x     0,0s  ok
fluxo-busca-corpo            1.099    13.265    12,1x     0,0s  ok
contexto-handle-diff           520     9.825    18,9x     0,0s  ok
contexto-arquivo-inteiro       420     1.536     3,7x     0,0s  ok
impacto-select               2.044    28.546    14,0x     0,0s  ok
esqueleto-watcher              103       686     6,7x     0,0s  ok
esqueleto-arquivo-grande     1.169     9.385     8,0x     0,0s  ok
corpo-select                   309     2.388     7,7x     0,0s  ok
mapa-grafo                   2.036    76.284    37,5x     0,0s  ok
assert-omitted-body            189    10.174    53,8x     0,0s  ok
porque-build-graph             114     7.475    65,6x     0,0s  ok
trace-handle-context           142     1.793    12,6x     0,0s  ok
repeticao-mesmo-trecho         577     9.385    16,3x     0,0s  ok
TOTAL                        9.907   178.139    18,0x     0,1s

schema tools/list: 3.210 tokens em 16 ferramentas (pago a cada requisição, fora das tarefas)
economia: 94,4% dos tokens que reler os arquivos declarados custaria
```

`fator` é `naïve / brain`: quantas vezes mais caro seria ler. Fator abaixo de
`1,0x` não é bug do bench — é o servidor custando **mais** que um `Read`, e é
exatamente o tipo de caso que precisa aparecer no relatório.

## Opções

| Flag | Padrão | Efeito |
|---|---|---|
| `--server` | `target/release/brain-tools` | Binário MCP medido. |
| `--root` | `.` | Raiz do repositório (vira `--root-dir` do servidor). |
| `--tasks` | `bench/tasks.json` | Lista de tarefas. |
| `--baseline` | `bench/baseline.json` | Baseline gravado por `--update`. |
| `--update` | — | Grava o resultado atual como baseline. |
| `--max-growth` | `10.0` | Percentual aceito antes de acusar regressão. |
| `--json` | — | Grava o relatório completo (com as chamadas) em JSON. |
| `--timeout-secs` | `300` | Teto por chamada MCP. |
| `--server-arg` | — | Repassa um argumento extra ao servidor. |
| `--verbose` | — | Mostra o stderr do servidor. |

Crescimento abaixo de 50 tokens não conta como regressão: a linha de
confiança do grafo e os contadores de candidatas mudam com o próprio
repositório.

Códigos de saída: `0` tudo dentro do teto, `1` regressão ou falha de execução
(serve como gate de CI, e é assim que `.github/workflows/build.yml` usa o
harness).

## Adicionar uma tarefa

```jsonc
{
  "id": "busca-orcamento",            // a-z, 0-9 e '-'; único
  "question": "Onde a resposta é truncada para caber no budget?",
  "naive": ["crates/brain-kernel/src/budget.rs"],   // arquivo ou diretório
  "calls": [
    { "tool": "brain_search",
      "arguments": { "natural_query": "…", "keywords": ["…"], "max_results": 3 } }
  ]
}
```

Boas tarefas:

- **uma pergunta que um agente faria de verdade**, com 1 a 3 chamadas;
- **baseline conservador**: prefira arquivos específicos a um diretório
  inteiro. Diretório é aceito (conta arquivo por arquivo, e o relatório lista
  quais), mas um baseline generoso faz a economia parecer maior do que é;
- **nada de saída dependente do relógio**: evite `brain_exec`, e não use
  `budget_tokens` minúsculo em tarefas que dependem do corte por handle.

Rode `--update` e **confira a tabela antes de commitar** o baseline: um número
bom demais costuma ser baseline mal declarado.

## Ler o resultado

- `TOTAL` e `economia` respondem "vale a pena?".
- `fator` por tarefa responde "onde vale mais?".
- `schema tools/list` responde "quanto custa manter 16 ferramentas expostas?".
- Regressões listam `[task]` e `[schema]` separadamente, porque encolher o
  schema vale mesmo que uma tarefa específica cresça.

O estado atual do repositório mostra os dois lados: `assert` e `why` pagam
~55–65× menos que reler o arquivo. `brain_context_graph` num arquivo inteiro
custava **mais** que abrir esse arquivo (0,8×); o digest de uma linha por símbolo
trocou o relatório repetido por fan-in/fan-out e o caso passou a pagar 3,7×.

Esse caso já tinha sido corrigido uma vez só na formatação — arestas duplicadas
e seções vazias repetidas — e o número não mudou: a resposta **saturava** o budget
de 2000 tokens, então remover ruído só fazia caber mais sinal no mesmo teto. É a
diferença entre "gasta menos" e "gasta melhor": o que resolveu foi mudar o desenho
da chamada, não o espaçamento.

Regra prática ao ler um número que não muda: se `brain` está colado no budget
(`2.000` + envelope), a ferramenta está truncando. Aí o ganho tem que aparecer
em outra tarefa, ou o desenho da chamada precisa mudar — não a formatação.

`repeticao-mesmo-trecho` mede o ledger de entrega: a primeira leitura custa 450
tokens, a segunda (o ponteiro com handle) 69 — 15,3% da primeira, e o valor
absoluto do ponteiro não cresce com o tamanho do símbolo. Para um corpo de 200
linhas a mesma segunda chamada custaria os mesmos ~69 tokens.
