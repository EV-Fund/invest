use axum::{
	Json,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use serde_json::json;
use tonic::{Code, Status};

use crate::session::SessionError;

/// The BFF's HTTP error surface. Renders as `{ "error": <message> }` with a status
/// mapped from the upstream gRPC code — matching the frontend's `httpStatusFor` /
/// `errorDetail` helpers.
#[derive(Debug)]
pub enum ApiError {
	/// No valid session (401).
	Unauthenticated,
	/// CSRF double-submit or `Origin` check failed (403).
	Csrf,
	/// Client input rejected before any upstream call (400).
	BadRequest(String),
	/// The session needs concierge and concierge cannot be reached (503). Nothing was
	/// closed: the next request may succeed.
	SessionUnavailable,
	/// A mutation failed upstream — surface invest-core's client-safe detail.
	Grpc(Status),
	/// A read failed upstream — map the code, but surface a fixed generic message
	/// (GET reads never leak upstream detail, e.g. "positions unavailable").
	ReadFailed { code: Code, message: String },
	/// A failure on this side — the session store, randomness (500). The report is logged
	/// and sent to error monitoring; the browser gets a fixed string.
	Internal(color_eyre::Report),
}

impl ApiError {
	/// Build a read failure: map the gRPC code to HTTP but use the given generic message.
	pub fn read(status: Status, message: &str) -> Self {
		ApiError::ReadFailed {
			code: status.code(),
			message: message.to_string(),
		}
	}
}

/// Codes invest-core uses for client-correctable conditions, whose `message()` is safe to
/// relay verbatim to the browser. Everything else (Internal/Unknown/Unavailable/DataLoss
/// and transport errors) is replaced with a generic string so internal detail never leaks.
fn client_safe(code: Code) -> bool {
	matches!(
		code,
		Code::InvalidArgument | Code::FailedPrecondition | Code::AlreadyExists | Code::NotFound | Code::PermissionDenied | Code::ResourceExhausted
	)
}

/// gRPC status code → HTTP status (mirrors `httpStatusFor` in the old TS BFF).
fn http_status_for(code: Code) -> StatusCode {
	match code {
		Code::Unauthenticated => StatusCode::UNAUTHORIZED,
		Code::PermissionDenied => StatusCode::FORBIDDEN,
		Code::InvalidArgument => StatusCode::BAD_REQUEST,
		Code::NotFound => StatusCode::NOT_FOUND,
		Code::AlreadyExists => StatusCode::CONFLICT,
		Code::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
		Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
		// invest-core answers Unavailable on a repository or ledger failure — transient and
		// retryable, unlike a broken upstream protocol (502).
		Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
		Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
		Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
		// Unknown/Internal/DataLoss stay 502 — a genuine upstream protocol failure.
		_ => StatusCode::BAD_GATEWAY,
	}
}

impl From<SessionError> for ApiError {
	fn from(err: SessionError) -> Self {
		match err {
			SessionError::Missing | SessionError::Rejected => ApiError::Unauthenticated,
			SessionError::Unavailable => ApiError::SessionUnavailable,
			SessionError::Internal(report) => ApiError::Internal(report),
		}
	}
}

impl From<Status> for ApiError {
	fn from(s: Status) -> Self {
		ApiError::Grpc(s)
	}
}

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		let (status, message) = match self {
			ApiError::Unauthenticated => (StatusCode::UNAUTHORIZED, "unauthenticated".to_string()),
			ApiError::Csrf => (StatusCode::FORBIDDEN, "csrf".to_string()),
			ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
			ApiError::SessionUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "sign-in is unavailable, try again".to_string()),
			ApiError::Internal(report) => {
				tracing::error!(error = ?report, "internal error behind a cabinet request");
				(StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
			}
			ApiError::ReadFailed { code, message } => (http_status_for(code), message),
			ApiError::Grpc(s) => {
				// Only codes invest-core uses to express client-correctable conditions carry their
				// `message()` through to the browser; the rest (Internal/Unknown/Unavailable/
				// DataLoss/transport errors) could embed DB/path/dependency detail, so they get
				// a fixed generic string and the real detail is logged server-side. The gate is
				// on the gRPC code alone — a read and a mutation with the same code leak the same.
				let detail = if client_safe(s.code()) {
					if s.message().is_empty() { "request failed".to_string() } else { s.message().to_string() }
				} else {
					tracing::warn!(code = ?s.code(), message = %s.message(), "upstream error withheld from client");
					"request failed".to_string()
				};
				(http_status_for(s.code()), detail)
			}
		};
		(status, Json(json!({ "error": message }))).into_response()
	}
}

#[cfg(test)]
mod tests {
	use axum::body::to_bytes;

	use super::*;

	async fn render(err: ApiError) -> (StatusCode, String) {
		let resp = err.into_response();
		let status = resp.status();
		let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		(status, body["error"].as_str().unwrap().to_string())
	}

	#[tokio::test]
	async fn internal_status_is_replaced_with_generic_message() {
		let leak = "sqlx: connection refused at postgres://hub:hunter2@10.0.0.5/bank";
		let (status, message) = render(ApiError::Grpc(Status::internal(leak))).await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(message, "request failed");
		assert!(!message.contains("postgres"));
	}

	#[tokio::test]
	async fn client_safe_status_message_passes_through() {
		let (status, message) = render(ApiError::Grpc(Status::invalid_argument("insufficient available balance"))).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert_eq!(message, "insufficient available balance");
	}

	#[tokio::test]
	async fn unavailable_transport_error_is_withheld() {
		let (status, message) = render(ApiError::Grpc(Status::unavailable("tcp connect error: 127.0.0.1:50051"))).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(message, "request failed");
	}

	#[test]
	fn grpc_codes_map_to_their_http_status() {
		let cases = [
			(Code::Unauthenticated, StatusCode::UNAUTHORIZED),
			(Code::PermissionDenied, StatusCode::FORBIDDEN),
			(Code::InvalidArgument, StatusCode::BAD_REQUEST),
			(Code::NotFound, StatusCode::NOT_FOUND),
			(Code::AlreadyExists, StatusCode::CONFLICT),
			(Code::FailedPrecondition, StatusCode::PRECONDITION_FAILED),
			(Code::ResourceExhausted, StatusCode::TOO_MANY_REQUESTS),
			(Code::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
			(Code::DeadlineExceeded, StatusCode::GATEWAY_TIMEOUT),
			(Code::Unimplemented, StatusCode::NOT_IMPLEMENTED),
			(Code::Unknown, StatusCode::BAD_GATEWAY),
			(Code::Internal, StatusCode::BAD_GATEWAY),
			(Code::DataLoss, StatusCode::BAD_GATEWAY),
		];
		for (code, expected) in cases {
			assert_eq!(http_status_for(code), expected, "{code:?}");
		}
	}
}
