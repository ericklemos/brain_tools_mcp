# Ledger de Entrega — não pagar duas vezes pelo mesmo trecho

**Data:** 2026-10-27
**Status:** Proposto — aguardando revisão
**Programa:** economia de tokens, medida por `crates/brain-bench`

## Contexto

O `brain-bench` mede o custo de **responder uma pergunta**. O custo de uma
sessão real é maior que a soma dessas respostas, porque o agente perguntas as
mesmas coisas mais de uma vez: reconfere um trecho antes de editar, refaz a
busca depois de um erro de compilação, reabre o arquivo que já recebeu.

Hoje nada do lado do servidor sabe o que já saiu. Quem tenta conter a
repetição é o hook `scripts/brain-read-once.py`, e ele só enxerga as
ferramentas nativas: `Read`, `Edit`, `Write`. Um `brain_read` repetido, um
`brain_search` com a mesma query ou um `brain_context_graph` no mesmo símbolo
são entregues de novo, inteiros, sem uma linha de aviso.

O daemon é **um processo por sessão do cliente** (stdio), então ele já tem o
escopo certo para lembrar: a sessão corrente. Falta a memória.

## Levantamento que moldou o desenho

**1. O ponto de registro é único.** Toda ferramenta monta `Vec<Section>` e
renderiza por `budget::select` + `budget::render`
(`crates/brain-kernel/src/budget.rs`). Registrar no `render` cobriria as dez
ferramentas de uma vez, mas ele não sabe qual `path`/faixa cada seção cobre.
Registrar na ferramenta, logo após o `select`, é o ponto que importa: **só é
"entregue" o que coube no contexto**; o que foi para o handle não foi visto.

**2. A infraestrutura de "guardar e apontar" já existe.** `ResponseStore`
(`crates/brain-kernel/src/store.rs`) guarda corpos por handle com TTL e teto de
bytes. O ledger não precisa inventar armazenamento: ele aponta para o handle
que o envelope já criou.

**3. O hash do arquivo já é calculado.** O builder mantém `file_hashes`
(blake3, `crates/brain-graph/src/builder.rs`) e o watcher o atualiza a cada
gravação. A entrega precisa do hash **no instante da entrega**, não do hash
atual — é o que separa "o agente já viu isto" de "o agente viu outra versão".

**4. Compactação de contexto é o risco real.** Quando o cliente compacta a
conversa, o agente perde o corpo que recebeu — e um ledger que responda
"já entregue" o deixaria sem nada. A saída não pode ser confiar no agente: a
omissão precisa carregar um **handle vivo**. Se o handle expirou, o corpo volta
inteiro, com o motivo declarado.

**5. O grafo já sabe a faixa.** `SymbolNode` carrega `start_line`/`end_line`,
mas linha não é chave estável: uma edição acima desloca tudo. A chave é byte
span; as linhas aparecem só no texto da resposta.

## Objetivo

Na mesma sessão, pedir de novo um trecho já entregue e inalterado custa uma
linha em vez do corpo; pedir um trecho que mudou custa só o diff desde a
entrega.

## Não-objetivos

- **Sem bloqueio.** O agente sempre pode pedir o corpo (`repeat: "allow"`); a
  omissão é informada, nunca silenciosa.
- **Sem persistência entre sessões.** Escopo é o processo, como o `ResponseStore`.
- **Sem deduplicação semântica.** Dois arquivos com o mesmo texto são dois
  arquivos; o ledger compara `(path, span, hash)`, não conteúdo.
- **Sem deduplicação dentro de uma resposta.** Colapsar candidatas quase
  idênticas do `brain_search` é outro problema, com outro desenho.
- **Não substitui o hook.** O hook cobre ferramentas nativas; o ledger cobre o
  MCP. Os dois são complementares e nenhum dos dois vê o outro.

## Desenho

### 1. `DeliveryLedger` (`crates/brain-kernel/src/deliveries.rs`)

```rust
pub struct Delivery<'a> {
    pub tool: &'a str,          // "brain_read", "brain_search", …
    pub path: &'a Path,         // o caminho canônico usado como chave
    pub id: &'a str,            // identidade do símbolo: `container::name` ou `name`
    pub file_hash: blake3::Hash,     // do arquivo inteiro no momento da entrega
    pub text: &'a str,               // o que foi entregue (para ponteiro e diff)
    pub complete: bool,              // `false` quando foi um recorte, não o corpo
    pub handle: Option<String>,
}

pub struct DeliveryLedger { /* LRU com teto de entradas e de bytes */ }

pub enum Verdict {
    /// Nunca entregue nesta sessão (ou o handle expirou).
    Fresh,
    /// Entregue, arquivo idêntico: pode omitir, apontando para o handle.
    AlreadySeen { handle: String, delivered_at: usize },
    /// Entregue, arquivo mudou: devolve só o diff da faixa.
    Changed { from: blake3::Hash, handle: Option<String> },
}

impl DeliveryLedger {
    pub fn check(
        &self,
        path: &Path,
        id: &str,
        file_hash: &blake3::Hash,
        current_text: &str,   // o símbolo como está no disco agora
        start_line: usize,    // para o cabeçalho `@@ N @@` do diff
    ) -> Verdict;
    pub fn record(&self, delivery: Delivery<'_>);
    pub fn note_omitted(&self, path: &Path, id: &str);
    pub fn remember_handle(&self, path: &Path, id: &str, handle: &str);
}
```

**A chave é a identidade do símbolo, não o byte span.** O desenho inicial usava
`start_byte..end_byte`; na implementação ficou claro que o span se desloca a
qualquer edição acima dele, e uma chave deslocada transformaria toda mudança em
"nunca vi" — o diff nunca aconteceria. `container::name` (ou `name` quando não
há container) sobrevive a edições e a linhas movidas.

O `brain_search` não conhece o container do snippet (`AstSnippet` só carrega o
nome), e dois `new` no mesmo arquivo compartilhariam um ponteiro. Por isso a
identidade dele é `name@start_line`: conservadora (uma linha movida vira "novo")
e incapaz de apontar para o símbolo errado.

Teto de entradas (default 512) e de bytes (default 8 MB de metadados). Fora do
teto, sai a entrega mais antiga — perder uma entrada só custa um corpo repetido,
nunca uma resposta errada.

### 2. Integração nas ferramentas

`brain_read` e `brain_search` — as duas ferramentas que entregam corpo — passam
a receber o ledger por builder, ao lado do `ResponseStore` já existente:

```rust
ReadTool::new(root, store, budget).with_deliveries(ledger.clone())
```

O registro acontece **depois** do `budget::select`, sobre as seções que
entraram. Cada ferramenta reporta a faixa que entregou:

- `brain_read` com `symbol`: o símbolo, com a identidade `container::name`;
- `brain_read` sem `symbol` (esqueleto): **não registra** — é assinatura, não corpo;
- `brain_search`: cada `AstSnippet`, com a identidade `name@start_line`;
- `brain_context_graph`, `brain_map` e `brain_diff`: assinaturas apenas — **não
  registram** corpo. O ledger serve para corpos, e registrar a assinatura de 30
  símbolos não economiza a pergunta seguinte.

### 3. Resposta no segundo pedido

Mesmo símbolo, arquivo idêntico:

```
### `verify_token` (`auth.rs:1-3`) · blake3 `a3f1…`
> **Já entregue nesta sessão** (`brain_read`, blake3 `a3f1…`, 12 linhas, ~80 tokens).
> Corpo omitido; recupere com `brain_peek` em `h_7c2a91bd`.
```

Mesmo símbolo, arquivo mudou:

```text
### `verify_token` (`auth.rs:1-3`) · blake3 `a3f1…`
> **Mudou desde a entrega** (blake3 `9c02…` → `a3f1…`): só as linhas alteradas abaixo.

  @@ 1 @@
   pub fn verify_token(raw: &str) -> bool {
  -    !raw.is_empty()
  +    !raw.trim().is_empty()
   }
```

O diff é por linha dentro do span, com 1 linha de contexto, e **nunca é
truncado**: um diff cortado lê como "nada mais mudou". Se ele sozinho passar do
budget, entra inteiro e o envelope declara o excesso — a mesma regra de
prioridade que já vale para a seção de maior peso.

### 4. Regras de decisão (conservadoras de propósito)

| Situação | Resposta |
|---|---|
| Mesmo `(path, id)`, mesmo hash, entrega completa | Omissão + handle. |
| Mesmo `(path, id)`, hash diferente, entrega completa | Diff do símbolo. |
| Mesmo `(path, id)`, hash diferente, corpo igual | Omissão + `> o arquivo mudou fora deste símbolo`. |
| Mesmo `(path, id)`, entrega anterior era recorte | Corpo inteiro + motivo declarado. |
| Handle expirado | O corpo guardado volta ao `ResponseStore` sob um handle novo; a omissão continua válida. |
| Terceira omissão seguida do mesmo símbolo | Corpo inteiro + `> terceira repetição…`; o contador reinicia. |
| Arquivo ilegível ou fora do root | Corpo/erro normal; o ledger não opina. |
| `repeat: "allow"` no pedido | Corpo inteiro, e a entrega é re-registrada. |

Duas escolhas mudaram em relação ao rascunho, as duas por honestidade:

- **recorte não vira ponteiro nem diff.** `brain_read` entrega o que o chunker
  deu; quando o corpo foi truncado, apontar para ele esconderia o resto. Nesse
  caso o corpo inteiro volta, com o motivo escrito.
- **handle expirado não derruba a omissão.** O ledger guarda o texto entregue
  (é dele que sai o diff), então consegue reabrir o handle. Um ponteiro vivo é
  melhor que um corpo repetido.

### 5. Configuração

| Flag | Padrão | Efeito |
|---|---|---|
| `--delivery-ledger` | `true` | `--delivery-ledger false` reproduz o comportamento anterior. |
| `--delivery-entries` | `512` | Entradas antes de despejar a mais antiga. |
| `--delivery-max-bytes` | `8388608` | Teto de metadados do ledger. |

O TTL do ledger é o mesmo do `ResponseStore` (`--handle-ttl-secs`): ponteiro
que aponta para o que já morreu não economiza nada — por isso o handle é
reaberto a partir do texto guardado antes de aparecer na resposta.

### 6. Interação com o hook e com `brain_peek`

O hook bloqueia a releitura de faixa já lida por `Read` **nativo**, com arquivo
inalterado; ele não vê o MCP. O ledger bloqueia a reentrega pelo MCP, com a
mesma política de hash. Em conjunto, cobrem as duas metades.

`brain_peek` continua sendo a recuperação de tudo: o corpo omitido pelo ledger
está lá, e é a única forma de o agente voltar ao texto sem pagar de novo.

## Arquivos

| Arquivo | Ação |
|---|---|
| `crates/brain-kernel/src/deliveries.rs` | novo — ledger, `Verdict`, tetos |
| `crates/brain-kernel/src/lib.rs` | `pub mod deliveries` |
| `crates/brain-kernel/src/config.rs` | três flags |
| `crates/brain-graph/src/read_tool.rs` | registra span do símbolo; responde omissão/diff |
| `crates/brain-search/src/search_tool.rs` | registra snippet; responde omissão/diff |
| `crates/brain-tools/src/main.rs` | cria o ledger e injeta |
| `bench/tasks.json` | tarefa `repeticao-mesmo-trecho` |

## Testes

**Unitários (`deliveries.rs`)**
- mesmo símbolo + mesmo hash → `Seen` com o handle;
- hash diferente → `Changed`, com o diff e os dois hashes;
- símbolo editado que muda de posição continua sendo o mesmo símbolo;
- mudança fora do símbolo → `Seen` com `file_changed`;
- recorte entregue antes → `Full` com o motivo (`PartialDelivery`/`PartialBefore`);
- terceira omissão → `Full(TooManyRepeats)`, e o contador reinicia;
- `remember_handle` devolve o handle novo no próximo `check`;
- LRU respeita entradas e bytes; a entrada recém-gravada não é despejada.

**Integração (`read_tool`, `search_tool`)**
- segunda leitura do mesmo símbolo custa uma linha e cita um handle vivo;
- após reescrever o arquivo, a segunda leitura é um diff e ele está completo;
- `repeat: "allow"` devolve o corpo e re-registra;
- o esqueleto (`brain_read` sem `symbol`) nunca é omisso nem registra;
- o corpo omitido continua recuperável por `brain_peek`;
- `--delivery-ledger false` reproduz o comportamento anterior.

**Bench**
- nova tarefa `repeticao-mesmo-trecho`: `brain_read` duas vezes no mesmo
  símbolo. Critério: a segunda chamada ≤ 15% da primeira. A tarefa entra no
  `bench/baseline.json` **depois** da implementação; antes dela, o baseline
  registraria justamente o custo que a feature elimina.

## Decisões e riscos

**Ganho esperado.** Numa sessão que repete 5 leituras de 300 tokens cada, são
~1.500 tokens por sessão — o mesmo tipo de ganho que o hook já entrega para as
ferramentas nativas.

**Risco: compactação.** O agente perde o contexto, o ledger acha que ele viu.
Mitigação em duas camadas: o handle vivo é citado na omissão, e a omissão só
acontece enquanto o handle existir. Quando o TTL vence, o corpo volta.

**Risco: falso positivo por arquivo mudando fora do daemon.** O hash é do
arquivo inteiro: qualquer mudança vira diff, nunca "já viu" sobre código novo.
O watcher mantém o grafo em dia, mas o ledger lê o hash na hora do pedido.

**Risco: o agente ignora a omissão e insiste.** Ele pode; o custo é a chamada
recusada, não a resposta errada. Se o pedido repetir mais de duas vezes a mesma
faixa, o ledger passa a entregar o corpo com a nota `> terceira repetição:
entregando o corpo para não bloquear você.` — bloqueio silencioso é pior que
token.

**Risco: dois spans do mesmo arquivo com hashes diferentes.** Impossível: o
hash é do arquivo. Entregas antigas do mesmo arquivo são invalidadas em bloco
quando o hash muda.

## Critério de sucesso

1. `repeticao-mesmo-trecho`: a segunda chamada custa **menos de 100 tokens**
   (o ponteiro) e não cresce com o tamanho do símbolo.
2. As tarefas existentes do `bench` não regridem.
3. Nenhuma resposta omite conteúdo sem dizer o que omitiu, quanto custa, como
   recuperar e por quê.

**Medido depois da implementação:** primeira leitura 479 tokens, segunda 98
(20,5% da primeira, com 69 deles no corpo do ponteiro e 29 no pedido). O
critério de 15% do rascunho era otimista para um símbolo de 20 linhas: o
ponteiro tem custo fixo (~70 tokens), então a razão melhora conforme o corpo
cresce — um símbolo de 200 linhas pagaria os mesmos ~98 na segunda chamada.
