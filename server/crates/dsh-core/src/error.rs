//! 统一错误类型（模块 00 约定）：ErrorKind 与对外错误码一一对应。

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::fmt;

/// 错误分类。对外映射（gRPC/HTTP）见模块 05 的错误映射表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// 非 leader，携带 leader_hint（SDK 跟随）
    LeaderRedirect,
    NotFound,
    Validation,
    PublishBlocked,
    VersionPruned,
    SessionInUse,
    SessionExpired,
    Forbidden,
    CycleRef,
    Conflict,
    NoDraft,
    LimitExceeded,
    Internal,
    Storage,
    Raft,
    Crypto,
}

impl ErrorKind {
    /// 对外错误码字符串（design-v3 §7）。
    pub fn code(&self) -> &'static str {
        match self {
            ErrorKind::LeaderRedirect => "ERR_LEADER_REDIRECT",
            ErrorKind::NotFound => "ERR_NOT_FOUND",
            ErrorKind::Validation => "ERR_VALIDATION",
            ErrorKind::PublishBlocked => "ERR_PUBLISH_BLOCKED",
            ErrorKind::VersionPruned => "ERR_VERSION_PRUNED",
            ErrorKind::SessionInUse => "ERR_SESSION_IN_USE",
            ErrorKind::SessionExpired => "ERR_SESSION_EXPIRED",
            ErrorKind::Forbidden => "ERR_FORBIDDEN",
            ErrorKind::CycleRef => "ERR_CYCLE_REF",
            ErrorKind::Conflict => "ERR_CONFLICT",
            ErrorKind::NoDraft => "ERR_NO_DRAFT",
            ErrorKind::LimitExceeded => "ERR_LIMIT_EXCEEDED",
            ErrorKind::Internal => "ERR_INTERNAL",
            ErrorKind::Storage => "ERR_STORAGE",
            ErrorKind::Raft => "ERR_RAFT",
            ErrorKind::Crypto => "ERR_CRYPTO",
        }
    }
}

/// 统一错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub detail: Option<JsonValue>,
    pub leader_hint: Option<String>,
    pub request_id: Option<String>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
            leader_hint: None,
            request_id: None,
        }
    }

    pub fn with_detail(mut self, detail: JsonValue) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn not_found(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::NotFound, format!("not found: {what}"))
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, message)
    }

    pub fn publish_blocked(detail: JsonValue) -> Self {
        Self::new(
            ErrorKind::PublishBlocked,
            "publish blocked by integrity checks",
        )
        .with_detail(detail)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::LimitExceeded, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// 携带 leader 转发提示（ERR_LEADER_REDIRECT 用）。
    pub fn with_leader_hint(mut self, hint: String) -> Self {
        self.leader_hint = Some(hint);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_codes_are_stable() {
        assert_eq!(ErrorKind::LeaderRedirect.code(), "ERR_LEADER_REDIRECT");
        assert_eq!(ErrorKind::NoDraft.code(), "ERR_NO_DRAFT");
        assert_eq!(ErrorKind::LimitExceeded.code(), "ERR_LIMIT_EXCEEDED");
    }

    #[test]
    fn error_helpers() {
        let e = Error::not_found("project x");
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert!(e.to_string().contains("ERR_NOT_FOUND"));

        let e = Error::publish_blocked(serde_json::json!({ "missing": ["a/b"] }));
        assert!(e.detail.is_some());
    }
}
