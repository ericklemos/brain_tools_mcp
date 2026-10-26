use brain_kernel::{BrainError, Result};
use brain_kernel::ast::SupportedLanguage;
use tree_sitter::Query;

pub struct GraphQueries {
    pub rust_defs: Query,
    pub rust_calls: Query,
    pub rust_imports: Query,
    /// Optional: receiver typing is an accuracy improvement, and a tree-sitter
    /// grammar change must not take the whole graph down with it.
    pub rust_bindings: Option<Query>,
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

        // `@call.receiver` is set only for `x.method()`: the receiver is what
        // decides which type's method this is, and without it every `build()`
        // looks like every other `build()`.
        let rust_calls_str = r#"
            (call_expression function: (identifier) @call.name)
            (call_expression function: (field_expression value: (identifier) @call.receiver field: (field_identifier) @call.name))
            (call_expression function: (scoped_identifier path: (identifier) @call.receiver name: (identifier) @call.name))
            (call_expression function: (scoped_identifier name: (identifier) @call.name) @call.path)
            (call_expression function: (scoped_identifier path: (scoped_identifier) @call.path name: (identifier) @call.name))
            (macro_invocation macro: (identifier) @call.name)
        "#;

        // Import paths drive scope resolution: without them a call can only be
        // matched by bare name, which links every `new()` to every `new()`.
        let rust_imports_str = r#"
            (use_declaration argument: (_) @import.path)
        "#;

        // Bindings name the type behind a receiver, so `builder.build()` can be
        // resolved to `GraphBuilder::build` instead of to every `build`. Covers
        // the three forms that actually appear: an annotated `let`, a `let`
        // initialised by `Type::new(…)`/`Type::default()`, and a typed
        // parameter (`fn drive(builder: &Builder)`).
        let rust_bindings_str = r#"
            (let_declaration pattern: (identifier) @bind.name type: (_) @bind.type)
            (let_declaration
                pattern: (identifier) @bind.name
                value: (call_expression function: (scoped_identifier path: (identifier) @bind.type)))
            (let_declaration
                pattern: (identifier) @bind.name
                value: (struct_expression name: (type_identifier) @bind.type))
            (parameter pattern: (identifier) @bind.name type: (_) @bind.type)
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
        let rust_imports = Query::new(&rust_lang, rust_imports_str)
            .map_err(|e| BrainError::Ast(format!("Rust imports query error: {e}")))?;

        // A grammar that rejects this costs receiver typing, nothing else.
        let rust_bindings = Query::new(&rust_lang, rust_bindings_str).ok();

        let kotlin_defs = Query::new(&kotlin_lang, kotlin_defs_str)
            .map_err(|e| BrainError::Ast(format!("Kotlin defs query error: {e}")))?;
        let kotlin_calls = Query::new(&kotlin_lang, kotlin_calls_str)
            .map_err(|e| BrainError::Ast(format!("Kotlin calls query error: {e}")))?;

        Ok(Self {
            rust_defs,
            rust_calls,
            rust_imports,
            rust_bindings,
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

    /// Import declarations. Kotlin has no query yet, so scoping there falls back
    /// to file-local resolution rather than inventing edges.
    pub fn imports_query(&self, lang: SupportedLanguage) -> Option<&Query> {
        match lang {
            SupportedLanguage::Rust => Some(&self.rust_imports),
            SupportedLanguage::Kotlin => None,
        }
    }
}
