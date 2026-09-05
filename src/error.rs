//! Application error type shared across all modules.
//!
//! Carries a process exit code (see README "Exit codes") and a
//! user-facing message. Conversions from library errors are provided
//! so `?` works directly on rusqlite/serde_json/io/self_update failures.

pub const EXIT_OK: i32 = 0;
pub const EXIT_RUNNING: i32 = 1;
pub const EXIT_NOT_FOUND: i32 = 2;
pub const EXIT_DB: i32 = 3;

#[derive(Debug)]
pub struct AppError {
    pub code: i32,
    pub message: String,
}

impl AppError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        AppError {
            code,
            message: message.into(),
        }
    }

    /// Bad arguments or target not found (exit code 2).
    pub fn usage(message: impl Into<String>) -> Self {
        AppError::new(EXIT_NOT_FOUND, message)
    }

    /// Database or environment failure (exit code 3).
    pub fn db(message: impl Into<String>) -> Self {
        AppError::new(EXIT_DB, message)
    }

    /// Guarded command refused while opencode is running (exit code 1).
    pub fn busy(message: impl Into<String>) -> Self {
        AppError::new(EXIT_RUNNING, message)
    }

    /// Exit with a code but no error line (usage already printed).
    pub fn silent(code: i32) -> Self {
        AppError::new(code, "")
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::db(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::db(e.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::db(e.to_string())
    }
}

impl From<self_update::Error> for AppError {
    fn from(e: self_update::Error) -> Self {
        AppError::db(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
