//! The error of a module call.

use std::fmt;

use obc_module_abi::{
    OBC_ERR_FAILED, OBC_ERR_INVALID, OBC_ERR_PANIC, OBC_ERR_STAGE, OBC_ERR_STATE,
    OBC_ERR_UNAVAILABLE, OBC_ERR_UNSUPPORTED, obc_status,
};

/// What kind of failure an [`Error`] is; each maps to an `OBC_ERR_*`
/// status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// An argument is invalid ([`OBC_ERR_INVALID`]): a null pointer, a
    /// structure too small, malformed JSON, an unknown stage or parameter.
    Invalid,
    /// Valid but not supported by this module ([`OBC_ERR_UNSUPPORTED`]):
    /// a frame kind, format or geometry it does not handle.
    Unsupported,
    /// What the call needs is not available here
    /// ([`OBC_ERR_UNAVAILABLE`]): a model, a runtime, the hardware.
    Unavailable,
    /// A run-time failure ([`OBC_ERR_FAILED`]).
    Failed,
    /// The call came in the wrong state ([`OBC_ERR_STATE`]), for example a
    /// frame before a successful load.
    State,
    /// The module's code panicked ([`OBC_ERR_PANIC`]); set by the SDK.
    Panic,
    /// One stage of the chain failed on its own, the rest of the chain
    /// intact ([`OBC_ERR_STAGE`]); made with [`Error::stage`], which names
    /// the stage. Only [`crate::Instance::load`] and
    /// [`crate::Instance::process`] may return it; see [`OBC_ERR_STAGE`] for
    /// what it promises about the chain.
    Stage,
}

impl ErrorKind {
    /// The status code of this kind.
    pub fn status(self) -> obc_status {
        match self {
            ErrorKind::Invalid => OBC_ERR_INVALID,
            ErrorKind::Unsupported => OBC_ERR_UNSUPPORTED,
            ErrorKind::Unavailable => OBC_ERR_UNAVAILABLE,
            ErrorKind::Failed => OBC_ERR_FAILED,
            ErrorKind::State => OBC_ERR_STATE,
            ErrorKind::Panic => OBC_ERR_PANIC,
            ErrorKind::Stage => OBC_ERR_STAGE,
        }
    }
}

/// The failure of a module call: a kind (the status code the daemon gets)
/// and a message (what `last_error` returns), written for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    /// The failed stage of an [`ErrorKind::Stage`] error.
    stage: Option<String>,
}

impl Error {
    /// An error of `kind`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
            stage: None,
        }
    }

    /// An [`ErrorKind::Stage`] error: stage `stage` of the chain failed on
    /// its own, for `message`. The daemon reads the stage id from
    /// `instance_last_error`, which is `<stage>: <message>`.
    pub fn stage(stage: impl Into<String>, message: impl Into<String>) -> Self {
        Error {
            kind: ErrorKind::Stage,
            message: message.into(),
            stage: Some(stage.into()),
        }
    }

    /// An [`ErrorKind::Invalid`] error.
    pub fn invalid(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Invalid, message)
    }

    /// An [`ErrorKind::Unsupported`] error.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Unsupported, message)
    }

    /// An [`ErrorKind::Unavailable`] error.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Unavailable, message)
    }

    /// An [`ErrorKind::Failed`] error.
    pub fn failed(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Failed, message)
    }

    /// An [`ErrorKind::State`] error.
    pub fn state(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::State, message)
    }

    /// The kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The failed stage of an [`ErrorKind::Stage`] error.
    pub fn failed_stage(&self) -> Option<&str> {
        self.stage.as_deref()
    }

    /// The text `last_error` and `instance_last_error` return: the message,
    /// after `<stage>: ` for an [`ErrorKind::Stage`] error.
    pub fn wire_message(&self) -> String {
        match &self.stage {
            Some(stage) => format!("{stage}: {}", self.message),
            None => self.message.clone(),
        }
    }

    /// The status code of the kind.
    pub fn status(&self) -> obc_status {
        self.kind.status()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    /// Malformed or mistyped JSON (a configuration that does not
    /// deserialise): [`ErrorKind::Invalid`].
    fn from(e: serde_json::Error) -> Self {
        Error::invalid(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_status_codes() {
        assert_eq!(Error::invalid("x").status(), OBC_ERR_INVALID);
        assert_eq!(Error::unsupported("x").status(), OBC_ERR_UNSUPPORTED);
        assert_eq!(Error::unavailable("x").status(), OBC_ERR_UNAVAILABLE);
        assert_eq!(Error::failed("x").status(), OBC_ERR_FAILED);
        assert_eq!(Error::state("x").status(), OBC_ERR_STATE);
        assert_eq!(Error::new(ErrorKind::Panic, "x").status(), OBC_ERR_PANIC);
        assert_eq!(Error::stage("relight", "x").status(), OBC_ERR_STAGE);
    }

    #[test]
    fn stage_errors_name_their_stage_on_the_wire() {
        let e = Error::stage("relight", "the light could not be set");
        assert_eq!(e.kind(), ErrorKind::Stage);
        assert_eq!(e.failed_stage(), Some("relight"));
        assert_eq!(e.message(), "the light could not be set");
        assert_eq!(e.wire_message(), "relight: the light could not be set");
        let plain = Error::failed("out of memory");
        assert_eq!(plain.failed_stage(), None);
        assert_eq!(plain.wire_message(), "out of memory");
    }

    #[test]
    fn json_errors_are_invalid() {
        let e: Error = serde_json::from_str::<u32>("nope").unwrap_err().into();
        assert_eq!(e.kind(), ErrorKind::Invalid);
        assert!(!e.message().is_empty());
        assert_eq!(e.to_string(), e.message());
    }
}
