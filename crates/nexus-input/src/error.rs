use thiserror::Error;

/// Errors produced while loading or saving compatible binding documents.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// The JSON document is malformed or has the wrong root type.
    #[error("input binding JSON is invalid")]
    InvalidJson(#[source] serde_json::Error),
    /// The XML document is malformed or has the wrong root element.
    #[error("game binding XML is invalid")]
    InvalidXml,
    /// A configured input-size limit was exceeded.
    #[error("binding document exceeds a configured limit")]
    LimitExceeded,
    /// A filesystem operation failed. Paths are intentionally not included.
    #[error("binding persistence I/O failed")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for PersistenceError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

/// Errors raised by the GW2 game-input state machine.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum GameInputError {
    /// No primary or secondary binding exists for the requested action.
    #[error("game action is not bound")]
    Unbound,
    /// The action is already held by this state machine.
    #[error("game action is already pressed")]
    AlreadyPressed,
    /// The injected platform sink reported a closed failure.
    #[error("game message sink failed")]
    SinkFailed,
    /// The injected platform sink panicked. The panic never crosses the boundary.
    #[error("game message sink panicked")]
    SinkPanicked,
}

/// Closed failure returned by a platform game-message sink.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("platform game-message dispatch failed")]
pub struct GameSinkError;
