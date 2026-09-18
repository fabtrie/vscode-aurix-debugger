use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Error of an operation (HTTP error responses, MCP tool errors).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    /// What went wrong, for programs.
    pub code: ErrorCode,
    /// What went wrong, for people.
    pub message: String,
}

/// Error kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Invalid arguments.
    BadRequest,
    /// Unknown target, register, breakpoint or operation.
    NotFound,
    /// The target must be halted (or running) for this.
    NotHalted,
    /// The target has not been examined (not accessible yet).
    NotExamined,
    /// Another client is using the target, e.g. a source-level step from VS Code runs.
    Busy,
    /// The target did not reach the requested state in time.
    Timeout,
    /// Memory access failed (bus error, protection).
    Memory,
    /// Debug adapter (TAS) connection or protocol error.
    Adapter,
    /// The device is locked (debug access disabled).
    DeviceLocked,
    /// The device is not accessible (power-down, reset active).
    DeviceAccess,
    /// Out of a resource, e.g. hardware breakpoints.
    Resource,
    /// The operation failed.
    Failed,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::BadRequest => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::NotHalted | ErrorCode::NotExamined | ErrorCode::Busy | ErrorCode::Resource => 409,
            ErrorCode::Memory => 422,
            ErrorCode::DeviceLocked => 423,
            ErrorCode::Failed => 500,
            ErrorCode::Adapter => 502,
            ErrorCode::DeviceAccess => 503,
            ErrorCode::Timeout => 504,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::NotFound => "not_found",
            ErrorCode::NotHalted => "not_halted",
            ErrorCode::NotExamined => "not_examined",
            ErrorCode::Busy => "busy",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Memory => "memory",
            ErrorCode::Adapter => "adapter",
            ErrorCode::DeviceLocked => "device_locked",
            ErrorCode::DeviceAccess => "device_access",
            ErrorCode::Resource => "resource",
            ErrorCode::Failed => "failed",
        }
    }
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError { code, message: message.into() }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::BadRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::NotFound, message)
    }

    pub fn failed(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorCode::Failed, message)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}
