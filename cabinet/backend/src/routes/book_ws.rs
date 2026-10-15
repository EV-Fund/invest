//! `GET /cabinet/api/book/ws?service=&depth=` — the live book feed for one allocation.
//!
//! A thin bridge from invest-core's `BookService.WatchBook` server-stream to the browser,
//! under the rules of [`super::ws`].
//! What differs is the payload: a frame here IS the book — the same snapshot `GET
//! /api/book` answers, the latest public trades, and `orders_revision`, the revision at
//! which the caller's own orders last changed. The client renders the snapshot as it comes
//! and refetches `/api/book/orders` only when `orders_revision` moves; no other user's
//! orders and no balances ride on the socket. Frames coalesce upstream, so a burst of
//! changes yields one frame with the latest state.
//!
//! The subscription carries the session's access token, as every `/book/*` route does, and
//! the socket lives no longer than that token.

use axum::{
	extract::{
		Query, State,
		ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
	},
	http::HeaderMap,
	response::Response,
};
use invest_contracts::invest::v1 as iv;
use serde_json::json;
use tokio::sync::mpsc;
use tonic::Status;

use crate::{
	cookies::Cookies,
	dto,
	error::ApiError,
	now_secs,
	routes::{
		book::{BookQuery, required_service},
		origin_allowed, require_session,
		ws::{CLOSE_FEED_UNAVAILABLE, CLOSE_NORMAL, CLOSE_TOKEN_EXPIRED, HEARTBEAT, close, time_left},
	},
	session::{Session, SocketSlot},
	state::{AppState, Grpc},
};

/// How many frames may sit between the upstream stream and the socket. Small on purpose:
/// every frame is a whole snapshot, so a slow browser should back-pressure the pump rather
/// than accumulate a backlog of books it will only ever render the last of.
const FRAME_BUFFER: usize = 4;

/// The handshake. Everything that can refuse this connection refuses it here, as a plain
/// HTTP status, before any upgrade.
pub async fn upgrade(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, Query(q): Query<BookQuery>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
	if !origin_allowed(&st, &headers) {
		return Err(ApiError::Csrf);
	}
	let service = required_service(q.service)?;
	// The session also tells how long its token is good for, which is how long the socket
	// may stay open.
	let session = require_session(&st, &jar).await?;
	// Taken before the upgrade and held by the socket's task, so a refused fifth socket costs
	// no upstream stream, and a socket that never upgrades frees its place on drop.
	let slot = st.sessions.claim_socket(session.key).ok_or(ApiError::TooMany)?;
	let depth = q.depth.unwrap_or(0);
	Ok(ws.on_upgrade(move |socket| serve(st, socket, session, slot, service, depth)))
}

/// One task per socket, holding both ends of the bridge; it returns — dropping the
/// upstream subscription with it — on client disconnect, upstream end, or token expiry.
async fn serve(st: AppState, mut socket: WebSocket, session: Session, _slot: SocketSlot, service: String, depth: u32) {
	let (token, expires_at) = (&session.access_token, session.access_expires_at);
	let mut feed = match watch(&st.grpc, token, &service, depth).await {
		Ok(feed) => feed,
		// The page still works without the socket: it polls. Saying so beats a socket that
		// looks alive and silently never moves.
		Err(status) => {
			tracing::warn!(code = ?status.code(), %service, "book feed unavailable; closing the socket");
			return close(socket, CLOSE_FEED_UNAVAILABLE, "book feed unavailable").await;
		}
	};

	let expiry = tokio::time::sleep(time_left(expires_at));
	tokio::pin!(expiry);
	let mut heartbeat = tokio::time::interval(HEARTBEAT);
	heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

	loop {
		// The socket is touched only OUTSIDE this block: `select!` holds every branch
		// future at once, so a branch that sent would collide with the one that reads.
		let event = tokio::select! {
			frame = feed.recv() => match frame {
				Some(frame) => Event::Upstream(Box::new(frame)),
				None => Event::UpstreamEnded,
			},
			_ = heartbeat.tick() => Event::Heartbeat,
			incoming = socket.recv() => match incoming {
				// Nothing the client says on this socket is input; only its going away is.
				// Ping frames are answered by axum from inside `recv`.
				None | Some(Err(_)) | Some(Ok(Message::Close(_))) => Event::Gone,
				Some(Ok(_)) => continue,
			},
			() = &mut expiry => Event::Expired,
		};

		let frame = match event {
			Event::Upstream(event) => book_frame(&(*event).into()),
			// The session is re-read on the keepalive: signed out, or closed by concierge,
			// the feed ends with it instead of running on for the token's last minutes
			// (security review F3). A store that cannot answer leaves the socket be.
			Event::Heartbeat => match st.sessions.is_live(&session.key, now_secs()).await {
				Ok(false) => return close(socket, CLOSE_TOKEN_EXPIRED, "session ended").await,
				Ok(true) => heartbeat_frame(now_secs()),
				Err(err) => {
					tracing::warn!(error = ?err, "could not re-read a book socket's session");
					heartbeat_frame(now_secs())
				}
			},
			// An ended upstream is a normal event (an invest-core replica rolling); the client
			// reconnects, and the first frame of the new subscription is a full snapshot.
			Event::UpstreamEnded => return close(socket, CLOSE_NORMAL, "feed ended").await,
			Event::Expired => return close(socket, CLOSE_TOKEN_EXPIRED, "access token expired").await,
			Event::Gone => return,
		};
		if socket.send(frame).await.is_err() {
			return;
		}
	}
}

enum Event {
	/// Boxed: a frame is a whole book, and the other four variants carry nothing.
	Upstream(Box<iv::BookEvent>),
	UpstreamEnded,
	Heartbeat,
	Expired,
	Gone,
}

/// Subscribe to one book's feed.
///
/// The upstream stream is established before this returns, so an invest-core that cannot serve
/// the feed is refused at the handshake rather than by a socket that opens and then never
/// moves. The pump task owns the stream and holds only the sender: when the socket drops
/// its receiver the next send fails and the task returns, so a browser that disconnects
/// takes its task and its subscription with it instead of leaking one per reconnect.
async fn watch(grpc: &Grpc, token: &str, service: &str, depth: u32) -> Result<mpsc::Receiver<iv::BookEvent>, Status> {
	let mut stream = grpc.watch_book(token, service, depth).await?;
	let (frames, receiver) = mpsc::channel(FRAME_BUFFER);

	tokio::spawn(async move {
		loop {
			match stream.message().await {
				Ok(Some(frame)) =>
					if frames.send(frame).await.is_err() {
						break;
					},
				Ok(None) => break,
				Err(status) => {
					tracing::warn!(code = ?status.code(), "book feed ended with an error");
					break;
				}
			}
		}
	});

	Ok(receiver)
}

/// One book frame: `type: "book"` plus the [`dto::BookEvent`] fields verbatim, so the
/// browser reads it with the same type it reads `GET /api/book` and the tape with.
fn book_frame(event: &dto::BookEvent) -> Message {
	Message::Text(Utf8Bytes::from(
		json!({
			"type": "book",
			"snapshot": event.snapshot,
			"trades": event.trades,
			"orders_revision": event.orders_revision,
		})
		.to_string(),
	))
}

/// The keepalive carries no book at all — only the time — so it can never be mistaken
/// for a change the client should render or refetch on.
fn heartbeat_frame(at: i64) -> Message {
	Message::Text(Utf8Bytes::from(json!({ "type": "heartbeat", "at": at.to_string() }).to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn payload(message: &Message) -> serde_json::Value {
		match message {
			Message::Text(text) => serde_json::from_str(text.as_str()).expect("a frame is JSON"),
			other => panic!("a book frame is text, got {other:?}"),
		}
	}

	fn event() -> iv::BookEvent {
		iv::BookEvent {
			snapshot: Some(iv::BookSnapshot {
				service: "quy-nhon".into(),
				revision: 42,
				bids: vec![iv::BookLevel {
					price: "100.00".into(),
					size: "5.0000".into(),
					orders: 2,
				}],
				asks: vec![],
				last_price: "101.25".into(),
				last_side: "sell".into(),
				mid: String::new(),
				spread: String::new(),
				nav: "99.80".into(),
				volume_24h: "12.5000".into(),
				change_24h: "-2.5".into(),
				as_of: 1_756_000_000,
			}),
			trades: vec![iv::Trade {
				id: "trade-1".into(),
				service: "quy-nhon".into(),
				price: "101.25".into(),
				size: "2.5000".into(),
				taker_side: "sell".into(),
				executed_at: 1_756_000_000,
				user_side: String::new(),
				order_id: String::new(),
				fee: String::new(),
			}],
			orders_revision: 7,
		}
	}

	/// A frame is the `BookEvent` DTO under a `type` tag and nothing else: the snapshot the
	/// REST read answers, the tape, and the caller's orders revision — with every 64-bit
	/// value a string, like the rest of this wire. The key set is pinned so no field is
	/// added here without a second look at what a socket may carry.
	#[test]
	fn a_book_frame_is_the_event_under_a_type_tag() {
		let value = payload(&book_frame(&event().into()));
		assert_eq!(value["type"], "book");
		assert_eq!(value["snapshot"]["revision"], "42", "the revision travels as a string");
		assert_eq!(value["snapshot"]["as_of"], "1756000000");
		assert_eq!(value["snapshot"]["bids"][0]["orders"], 2);
		assert_eq!(value["snapshot"]["nav"], "99.80");
		assert_eq!(value["trades"][0]["taker_side"], "sell");
		assert_eq!(value["trades"][0]["user_side"], "", "the public tape names no party");
		assert_eq!(value["orders_revision"], "7");
		let keys: Vec<&str> = value.as_object().expect("an object").keys().map(String::as_str).collect();
		assert_eq!(keys, vec!["orders_revision", "snapshot", "trades", "type"]);
	}

	/// The keepalive carries no snapshot, so a client keyed on `type` never renders it as
	/// a book and never reads a moved revision off it.
	#[test]
	fn a_heartbeat_carries_no_book() {
		let value = payload(&heartbeat_frame(1_756_000_000));
		assert_eq!(value["type"], "heartbeat");
		assert_eq!(value["at"], "1756000000");
		assert!(value.get("snapshot").is_none() && value.get("orders_revision").is_none());
	}
}
