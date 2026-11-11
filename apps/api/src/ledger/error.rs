//! The error a ledger command fails with, in the shape the API answers with.

use super::*;

#[derive(Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub field: Option<&'static str>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            field: None,
        }
    }

    /// A request that failed validation, naming the field at fault.
    pub fn invalid(e: impl FieldError) -> Self {
        Self::bad_request(e.to_string()).in_field(e.field())
    }

    /// The same error, naming the request field at fault.
    pub fn in_field(self, field: &'static str) -> Self {
        Self {
            field: Some(field),
            ..self
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(404, code, message)
    }

    pub fn body(&self) -> serde_json::Value {
        let mut e = serde_json::json!({ "code": self.code, "message": self.message });
        if let Some(field) = self.field {
            e["field"] = field.into();
        }
        serde_json::json!({ "error": e })
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        Self::new(500, "internal_error", e.to_string())
    }
}
