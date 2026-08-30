//! One error type for the whole HTTP surface, so every handler can be written as
//! `-> ApiResult<Json<T>>` and use `?` on store/serde calls.
//!
//! Why a single enum rather than per-handler `(StatusCode, Json<Value>)` tuples:
//! this app has ~60 routes across six later-owned modules. A tuple-returning
//! convention makes every one of those handlers re-implement its own error mapping,
//! which is exactly how a 500 ends up leaking a SQL string to the frame. Funnelling
//! through one `IntoResponse` gives a single place where the status code, the stable
//! machine-readable `code`, and the message-vs-detail split are decided.
//!
//! Wire shape is fixed and snake_case, matching the rest of the sidecar:
//! `{ "error": "<human message>", "code": "<machine code>" }` — plus, for
//! [`ApiError::Validation`] only, a `fields` array carrying the per-field reasons a
//! record write was rejected. That extra key is what lets the panel highlight the
//! offending cells instead of showing one opaque toast.
//!
//! There is deliberately NO `Upstream`/502 variant: this app makes no outbound
//! calls of its own (email/calendar/enrichment are out of scope for v1), so every
//! failure is either the caller's or ours. Do not add one speculatively — a status
//! nothing constructs is a status the UI branches on and never sees.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::models::FieldValidationError;

/// Every handler in `crate::api` returns this.
pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    /// The addressed row does not exist (or is soft-deleted and the caller did not
    /// ask for deleted rows).
    NotFound(String),
    /// The caller's payload is structurally wrong — an unknown field type, an
    /// unparseable filter operator, a CSV with no rows.
    BadRequest(String),
    /// The row exists but is not in a state that admits this transition — merging a
    /// record into itself, deleting a standard object, deleting the last default
    /// view. Distinct from `BadRequest` because the payload was well formed.
    Conflict(String),
    /// A record write failed field-level validation. Carries every reason at once,
    /// not just the first: a form with four bad cells must light up four cells.
    Validation(Vec<FieldValidationError>),
    /// A route whose handler is owned by a module that has not landed yet. Returns
    /// 501 rather than 500 so the panel (and any smoke test) can tell "not built"
    /// from "broken", and so a monitoring alert on 5xx does not fire on known gaps.
    #[allow(dead_code)]
    NotImplemented(String),
    /// Anything else. The `anyhow` chain is logged in full; the client gets a fixed
    /// string, because these messages contain SQL and file paths.
    Internal(anyhow::Error),
}

impl ApiError {
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn bad_request(why: impl Into<String>) -> Self {
        Self::BadRequest(why.into())
    }

    pub fn conflict(why: impl Into<String>) -> Self {
        Self::Conflict(why.into())
    }

    /// Reject a record write with the full set of per-field reasons. Built from
    /// `ValidatedValues::errors`, which is why the store's validator collects rather
    /// than short-circuits.
    pub fn validation(errors: Vec<FieldValidationError>) -> Self {
        Self::Validation(errors)
    }

    /// The marker a later agent's module returns until its body lands. Kept as a
    /// constructor rather than a bare string so `grep -rn "not_implemented"` finds
    /// every remaining gap in one pass.
    #[allow(dead_code)]
    pub fn not_implemented(what: impl Into<String>) -> Self {
        Self::NotImplemented(what.into())
    }

    /// The stable machine-readable discriminator. The panel branches on this, never
    /// on the human message, so the message stays free to change.
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Conflict(_) => "conflict",
            Self::Validation(_) => "validation_failed",
            Self::NotImplemented(_) => "not_implemented",
            Self::Internal(_) => "internal_error",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            // 422, not 400: the request was syntactically fine and the route
            // matched — the *content* of the value bag is what failed. The panel
            // uses the split to decide between "you sent junk" and "fix these
            // cells".
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) => write!(f, "{m} not found"),
            Self::BadRequest(m) | Self::Conflict(m) => write!(f, "{m}"),
            Self::Validation(errors) => {
                let n = errors.len();
                let first = errors
                    .first()
                    .map(|e| format!("{}: {}", e.field_slug, e.message))
                    .unwrap_or_else(|| "invalid values".to_string());
                if n > 1 {
                    write!(f, "{first} (and {} more field errors)", n - 1)
                } else {
                    write!(f, "{first}")
                }
            }
            Self::NotImplemented(m) => write!(f, "{m} is not implemented yet"),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log the FULL chain before narrowing what the client sees. For `Internal`
        // this is the only place the real cause is ever recorded.
        if let Self::Internal(e) = &self {
            tracing::error!(error = ?e, "ryu-crm: internal error");
        } else {
            tracing::debug!(error = %self, code = self.code(), "ryu-crm: request rejected");
        }
        let status = self.status();
        let code = self.code();
        // `Internal` deliberately does NOT forward `e` — see the variant's doc.
        let message = match &self {
            Self::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        let mut body = json!({ "error": message, "code": code });
        if let Self::Validation(errors) = &self {
            body["fields"] = serde_json::to_value(errors).unwrap_or(serde_json::Value::Null);
        }
        (status, Json(body)).into_response()
    }
}

// ── `?` conversions ────────────────────────────────────────────────────────────
//
// The store returns `anyhow::Result`, so `From<anyhow::Error>` is what makes every
// handler's `?` work. The `rusqlite`/`serde_json` conversions exist so a module that
// touches those crates directly (an import handler decoding a mapping, an insights
// handler running its own aggregate) does not have to `.map_err(anyhow::Error::from)`
// at each call site.

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(anyhow::Error::from(e))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(anyhow::Error::from(e))
    }
}

/// A field-level rejection raised outside the store (an import row, a merge
/// resolution) still funnels into the 422 shape rather than a 400 string.
impl From<Vec<FieldValidationError>> for ApiError {
    fn from(errors: Vec<FieldValidationError>) -> Self {
        Self::Validation(errors)
    }
}

impl From<FieldValidationError> for ApiError {
    fn from(error: FieldValidationError) -> Self {
        Self::Validation(vec![error])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_codes_are_stable() {
        assert_eq!(
            ApiError::not_found("record").status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(ApiError::not_found("record").code(), "not_found");
        assert_eq!(ApiError::conflict("x").status(), StatusCode::CONFLICT);
        assert_eq!(
            ApiError::not_implemented("imports").status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            ApiError::validation(vec![]).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(ApiError::validation(vec![]).code(), "validation_failed");
    }

    #[test]
    fn internal_errors_do_not_leak_their_cause_to_the_client() {
        let err = ApiError::Internal(anyhow::anyhow!("SELECT * FROM records failed"));
        let message = match &err {
            ApiError::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        assert_eq!(message, "internal error");
    }

    #[test]
    fn validation_message_names_the_first_field_and_counts_the_rest() {
        let err = ApiError::validation(vec![
            FieldValidationError::new("fld_a", "email", "not a valid email address"),
            FieldValidationError::new("fld_b", "amount", "expected a number"),
        ]);
        assert_eq!(
            err.to_string(),
            "email: not a valid email address (and 1 more field errors)"
        );
    }
}
