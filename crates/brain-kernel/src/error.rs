use thiserror::Error;

#[derive(Error, Debug)]
pub enum BrainError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Search error: {0}")]
    Search(String),

    #[error("AST parsing error: {0}")]
    Ast(String),

    #[error("Model inference error: {0}")]
    Inference(String),

    #[error("Graph error: {0}")]
    Graph(String),

    #[error("MCP protocol error ({code}): {message}")]
    Mcp { code: i32, message: String },
}

pub type Result<T> = std::result::Result<T, BrainError>;
