//! Rendering helpers shared by the tools.
//!
//! Signatures come from tree-sitter with the source formatting intact, so a
//! method with four parameters occupies five lines. That is useful in the file,
//! but in an outline it is pure token overhead: the same signature reads fine
//! on one line and is not the verbatim body the contract promises.

/// Renders a multi-line signature as one line.
///
/// Only line breaks and their surrounding indentation are collapsed; spacing
/// inside a line is preserved, so `fn f(a: u8, b: u8)` stays unambiguous.
pub fn compact_signature(signature: &str) -> String {
    let mut out = String::with_capacity(signature.len());
    for line in signature.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_signature_becomes_one_line() {
        let signature = "fn affected_tests<'a>(\n        &self,\n        graph: &'a ProjectCodeGraph,\n    ) -> Vec<&'a SymbolNode>";
        assert_eq!(
            compact_signature(signature),
            "fn affected_tests<'a>( &self, graph: &'a ProjectCodeGraph, ) -> Vec<&'a SymbolNode>"
        );
    }

    #[test]
    fn intra_line_spacing_is_preserved() {
        assert_eq!(
            compact_signature("pub fn f(a: u8, b: u8) -> u8"),
            "pub fn f(a: u8, b: u8) -> u8"
        );
    }

    #[test]
    fn an_empty_signature_stays_empty() {
        assert_eq!(compact_signature("  \n  "), "");
    }
}
