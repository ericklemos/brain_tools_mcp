//! Guidance sent once at `initialize`.
//!
//! This is the only cross-tool text a client pays for on every session, so the
//! tokens go to routing: which question goes to which tool, and in what order.
//! What a single tool does stays in its schema, which the client receives on
//! `tools/list`.
//!
//! The text follows the tools the session actually exposes. Teaching
//! `brain_edit` to a `--tools read` session buys a failed call and a retry.

/// One routing line, shown only when its tool is in the session's profile.
const ROUTES: &[(&str, &str)] = &[
    ("brain_search", "não sabe onde está → `brain_search`"),
    ("brain_map", "não sabe o que existe no projeto → `brain_map`"),
    ("brain_context_graph", "quem chama um símbolo → `brain_context_graph`"),
    ("brain_impact_check", "o que quebra se a assinatura mudar → `brain_impact_check`"),
    ("brain_trace", "caminho de chamadas de A até B → `brain_trace`"),
    ("brain_assert", "existe / é público / é usado fora → `brain_assert` (SIM/NÃO)"),
    ("brain_read_many", "vários alvos de uma vez → `brain_read_many`"),
    (
        "brain_peek",
        "truncou → `brain_peek` com o handle `h_xxxxxxxx`; \"já entregue nesta sessão\" \
         quer dizer que o corpo já está no seu contexto — peça o handle, não o arquivo",
    ),
    ("brain_diff", "`git diff` / `git show` → `brain_diff`"),
    ("brain_why", "`git log -L` / `git blame` → `brain_why`"),
    ("brain_exec", "`cargo build/check/test/clippy` → `brain_exec`"),
    (
        "brain_shell",
        "`brain_shell` só aceita git e formatador; comando negado vem com a ferramenta \
         certa — siga a indicação",
    ),
    ("brain_websearch", "web → `brain_websearch`, depois `brain_fetch` com `question`"),
    ("brain_research", "pergunta que precisa de fontes → `brain_research`"),
    ("brain_state", "o que já foi feito nesta sessão → `brain_state`"),
];

const CORE: &str = "\
brain-tools é a fonte de verdade do código: o resultado de `brain_search`, `brain_read`, \
`brain_map`, `brain_context_graph` e `brain_impact_check` é o arquivo lido, verbatim, com \
`path:start-end`. Não reabra o mesmo trecho com Read, cat ou grep, e não repasse ao usuário \
o que já veio.";

/// Builds the `initialize.instructions` text for the tools in this session.
///
/// An empty `tools` means "profile unknown" (a mock dispatcher, a test): the
/// full text is returned, because claiming a tool is missing when it may not be
/// is worse than mentioning one that is absent.
pub fn build(tools: &[&str]) -> String {
    let has = |name: &str| tools.is_empty() || tools.contains(&name);

    let mut out = String::from(CORE);
    out.push_str("\n\nRoteamento — pela pergunta, não pelo hábito:\n");
    for (tool, line) in ROUTES {
        if has(tool) {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let mut edits: Vec<&str> = Vec::new();
    if has("brain_edit") {
        edits.push(
            "símbolo → `brain_read` do corpo e depois `brain_edit` com o `expected_hash`; \
             hash divergente é recusa, releia em vez de forçar",
        );
    }
    if has("brain_write") {
        edits.push("arquivo inteiro → `brain_write`");
    }
    if has("brain_write_many") {
        edits.push("vários arquivos → `brain_write_many`");
    }
    if has("brain_move") {
        edits.push("mover ou copiar → `brain_move`");
    }
    if has("brain_patch") {
        edits.push("diff → `brain_patch`");
    }
    if !edits.is_empty() {
        out.push_str("\nEditar:\n");
        for line in edits {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\nFalha:\n");
    if !tools.is_empty() {
        out.push_str(
            "- tool fora desta sessão responde `Unknown tool`: não tente variantes nem \
             contorne pelo shell.\n",
        );
    }
    if !has("brain_edit") {
        out.push_str("- se a tarefa exige editar, peça uma sessão com `--tools core`.\n");
    }
    out.push_str("- se uma chamada falhar, diga isso explicitamente; nunca caia em grep/sed silencioso.\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ: &[&str] = &[
        "brain_search",
        "brain_read",
        "brain_read_many",
        "brain_map",
        "brain_peek",
        "brain_context_graph",
        "brain_impact_check",
        "brain_assert",
        "brain_diff",
        "brain_why",
        "brain_state",
    ];
    const CORE: &[&str] = &[
        "brain_search",
        "brain_context_graph",
        "brain_impact_check",
        "brain_peek",
        "brain_exec",
        "brain_read",
        "brain_read_many",
        "brain_edit",
        "brain_write",
        "brain_write_many",
        "brain_move",
        "brain_patch",
        "brain_map",
        "brain_state",
    ];

    #[test]
    fn a_read_only_session_is_not_taught_to_write() {
        let text = build(READ);
        for absent in ["brain_edit", "brain_write", "brain_move", "brain_shell"] {
            assert!(!text.contains(absent), "`{absent}` taught in a read session: {text}");
        }
        assert!(text.contains("--tools core"), "missing the way out: {text}");
    }

    #[test]
    fn a_session_that_can_edit_learns_the_read_before_edit_handshake() {
        let text = build(CORE);
        assert!(text.contains("expected_hash"), "{text}");
        assert!(!text.contains("--tools core"), "asks for a profile it already has: {text}");
    }

    #[test]
    fn an_unknown_profile_keeps_every_route() {
        let text = build(&[]);
        assert!(text.contains("brain_edit") && text.contains("brain_shell"), "{text}");
        assert!(!text.contains("--tools core"), "{text}");
    }

    #[test]
    fn the_delivery_ledger_rule_travels_with_every_real_profile() {
        for tools in [READ, CORE] {
            let text = build(tools);
            assert!(text.contains("já entregue nesta sessão"), "{text}");
        }
    }

    #[test]
    fn the_guidance_stays_cheap_enough_to_send_every_session() {
        for tools in [&[][..], READ, CORE] {
            let len = build(tools).len();
            assert!(len < 2000, "instructions grew to {len} chars");
        }
    }
}
