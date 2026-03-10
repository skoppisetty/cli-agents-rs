/// Errors returned by cli-agents.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("no AI CLI found — install claude, codex, or gemini")]
    NoCli,

    #[error("must specify `cli` when using `executable_path`")]
    CliRequiredWithExecutable,

    #[error("CLI process failed: {0}")]
    Process(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
