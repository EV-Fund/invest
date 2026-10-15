use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use serde_json::json;

use crate::state::AppState;

/// `GET /cabinet/api/health/live` — the process is up and serving: no upstream call, no
/// state.
///
/// THE path for the pod's kubelet probes (liveness, readiness, startup). The deep check
/// [`health`] dials invest-core and Postgres, so with either mid-rollout it answers 502,
/// and a probe wired to it would pull the only BFF replica off the Service through no
/// fault of its own — the cabinet's Next proxy then meets ECONNREFUSED and answers a
/// bodyless 500 where the BFF would have answered a JSON error the UI can render.
pub async fn live() -> Json<serde_json::Value> {
	Json(json!({ "ok": true }))
}

/// `GET /cabinet/api/health` — the smoke path: invest-core's `grpc.health.v1` and the
/// session store. A dependency check for a human or a monitor, never a probe — see [`live`].
pub async fn health(State(st): State<AppState>) -> Response {
	let core = st.grpc.check().await;
	let store = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(st.sessions.pool()).await;
	match (core, store) {
		(Ok(true), Ok(_)) => Json(json!({ "ok": true })).into_response(),
		(core, store) => {
			// Log-then-withhold: a transport error can embed addresses, so the browser gets a
			// fixed string.
			tracing::warn!(core = ?core.map_err(|s| s.code()), store = ?store.err(), "health check failed");
			(StatusCode::BAD_GATEWAY, Json(json!({ "ok": false, "error": "upstream unavailable" }))).into_response()
		}
	}
}

/// The probe path, through the real router and its layers, with every dependency
/// unreachable.
#[cfg(test)]
mod probe_tests {
	use axum::{
		body::{Body, to_bytes},
		http::{Request, StatusCode},
	};
	use tower::ServiceExt;

	use crate::routes::{router, test_support};

	async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
		let request = Request::builder().uri(uri).body(Body::empty()).expect("build the request");
		let response = router(test_support::unreachable_state()).oneshot(request).await.expect("the router responds");
		let status = response.status();
		let body = to_bytes(response.into_body(), 1 << 16).await.expect("read the body");
		(status, serde_json::from_slice(&body).expect("a JSON body"))
	}

	/// The kubelet's question is "is the process alive" — the answer must not depend on
	/// invest-core being up, or an invest-core rollout takes the BFF's only replica with it.
	#[tokio::test]
	async fn live_answers_ok_with_every_dependency_unreachable() {
		let (status, body) = get("/cabinet/api/health/live").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body, serde_json::json!({ "ok": true }));
	}

	/// The contrast that makes the split necessary: the deep check does dial out and reports
	/// the dependency down — right for a smoke check, ruinous for a probe.
	#[tokio::test]
	async fn deep_health_reports_the_unreachable_dependency() {
		let (status, body) = get("/cabinet/api/health").await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["ok"], serde_json::json!(false));
	}
}
