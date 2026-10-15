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
