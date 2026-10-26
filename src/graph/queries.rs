use crate::error::{BrainError, Result};
use crate::search::ast_chunker::SupportedLanguage;
use tree_sitter::Query;

pub struct GraphQueries {
    pub rust_defs: Query,
    pub rust_calls: Query,
    pub kotlin_defs: Query,
    pub kotlin_calls: Query,
}

impl GraphQueries {
    pub fn new() -> Result<Self> {
        let rust_lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let kotlin_lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();

        let rust_defs_str = r#"
            (function_item name: (identifier) @name) @def
            (struct_item name: (type_identifier) @name) @def
            (enum_item name: (type_identifier) @name) @def
            (trait_item name: (type_identifier) @name) @def
        "#;

        let rust_calls_str = r#"
            (call_expression function: (identifier) @call.name)
            (call_expression function: (field_expression field: (field_identifier) @call.name))
            (call_expression function: (scoped_identifier name: (identifier) @call.name))
        "#;

        let kotlin_defs_str = r#"
            (function_declaration name: (identifier) @name) @def
            (class_declaration name: (identifier) @name) @def
            (object_declaration name: (identifier) @name) @def
        "#;

        let kotlin_calls_str = r#"
            (call_expression (identifier) @call.name)
            (call_expression (navigation_expression (identifier) @call.name))
        "#;

        let rust_defs = Query::new(&rust_lang, rust_defs_str)
            .map_err(|e| BrainError::Ast(format!("Rust defs query error: {e}")))?;
        let rust_calls = Query::new(&rust_lang, rust_calls_str)
            .map_err(|e| BrainError::Ast(format!("Rust calls query error: {e}")))?;

        let kotlin_defs = Query::new(&kotlin_lang, kotlin_defs_str)
            .map_err(|e| BrainError::Ast(format!("Kotlin defs query error: {e}")))?;
        let kotlin_calls = Query::new(&kotlin_lang, kotlin_calls_str)
            .map_err(|e| BrainError::Ast(format!("Kotlin calls query error: {e}")))?;

        Ok(Self {
            rust_defs,
            rust_calls,
            kotlin_defs,
            kotlin_calls,
        })
    }

    pub fn defs_query(&self, lang: SupportedLanguage) -> &Query {
        match lang {
            SupportedLanguage::Rust => &self.rust_defs,
            SupportedLanguage::Kotlin => &self.kotlin_defs,
        }
    }

    pub fn calls_query(&self, lang: SupportedLanguage) -> &Query {
        match lang {
            SupportedLanguage::Rust => &self.rust_calls,
            SupportedLanguage::Kotlin => &self.kotlin_calls,
        }
    }
}
