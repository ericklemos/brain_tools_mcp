# Web benchmark — bounded evidence vs whole page

```bash
cargo build --bin brain-bench-web --bin brain-tools
./target/debug/brain-bench-web \
  --corpus bench/web.json \
  --root /tmp/brain-web-bench \
  --server ./target/debug/brain-tools \
  --budget 800
```

## What it measures

For every URL the harness makes three MCP calls against a real `brain-tools`
process:

1. `brain_fetch` with a question and a token budget — the product being measured.
2. `brain_fetch` with no question and a 1M-token budget — **the whole page through
   the same extractor**, which is what a plain fetch tool delivers.
3. the first call again, to measure the cache path.

Because both sides share the extractor, the ratio isolates the variable this
project claims to change: selection and budget. It is deliberately **not** a
comparison against OpenCode's converter — doing that requires running their
tool, and mixing two variables would make the number meaningless.

Recall is checked against a target string per URL. If the string is absent from
the *full* page, the entry is reported as `corpus` and excluded: a wrong
expectation is not a tool failure.

## Result (2026-10-29, 30 URLs, budget 800)

| metric | value |
|---|---|
| URLs válidas | 30/30 |
| tokens, página inteira | 240 309 |
| tokens, evidência com orçamento | 23 045 |
| redução | **10,4x** |
| recall da evidência-alvo | **100%** |
| latência p50 / p95 (rede) | 289 ms / 1 380 ms |
| latência p50 (cache) | 1 ms |

Extremos: `dockerfile` 37 141 → 124 tokens (299x); `dom-smoothie` 330 → 344
(1,0x — página pequena, o orçamento não morde).

## Reading the numbers honestly

- The budget is a ceiling, not a target: pages already under 800 tokens come out
  slightly *larger* because of the provenance header.
- 100% recall here means the target string survived; it does not prove the best
  block was ranked first, only that the evidence was not lost.
- The cache latency is the second read of the same URL in the same process; it
  measures revalidation, not a cross-session cache.
