//! The book routes — the secondary market in an allocation's units, behind `/api/book/*`.
//!
//! Every handler forwards the user's access token to invest-core. Reads answer with a fixed
//! generic message on failure, mutations relay invest-core's client-safe wording — the
//! same split as the fund routes. The string vocabularies a browser may send (`side`,
//! `kind`, `tif`, `resolution`) are checked here against `invest_contracts::book`, so a
//! client bug is refused one hop away with the list of words that would have worked.
//!
//! The live feed is [`super::book_ws`].

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use invest_contracts::{
	book::{kind as wire_kind, resolution as wire_resolution, side as wire_side, tif as wire_tif},
	invest::v1 as iv,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
	cookies::Cookies,
	dto,
	error::ApiError,
	routes::{editable, parse_body, require_token, required, verify_csrf},
	state::AppState,
};

/// `?service=&depth=` — the book's identity and how many levels a side to render.
#[derive(Deserialize)]
pub struct BookQuery {
	pub service: Option<String>,
	pub depth: Option<u32>,
}

/// `?service=&limit=` — a tape or a history page. `service` is optional on the caller's
/// own lists (absent = every allocation) and required on the public tape.
#[derive(Deserialize)]
pub struct PageQuery {
	service: Option<String>,
	limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ServiceQuery {
	service: Option<String>,
}

#[derive(Deserialize)]
pub struct CandlesQuery {
	service: Option<String>,
	resolution: Option<String>,
	from: Option<i64>,
	to: Option<i64>,
}

/// The one allocation a per-book read names. A missing or blank service is a client error
/// decided here — never a call to invest-core with an empty id.
pub fn required_service(service: Option<String>) -> Result<String, ApiError> {
	service.filter(|s| !s.trim().is_empty()).ok_or_else(|| ApiError::BadRequest("service is required".into()))
}

/// A word the contract pins: present, and one of `expected`. The refusal names the
/// vocabulary, so a client that sent `BUY` or `fok` learns what would have worked.
fn vocabulary(v: &Value, key: &str, expected: &[&str]) -> Result<String, ApiError> {
	let Some(word) = required(v, key) else {
		return Err(ApiError::BadRequest(format!("{key} is required — one of {}", expected.join(", "))));
	};
	if !expected.contains(&word.as_str()) {
		return Err(ApiError::BadRequest(format!("unknown {key} '{word}' — expected one of {}", expected.join(", "))));
	}
	Ok(word)
}

// ── the book (readable by anyone who may view the allocation) ────────────────

/// `GET /api/book?service=&depth=` — the aggregated book: the top `depth` levels a side,
/// the last trade, the 24h figures and NAV for the ticker. A hidden allocation is a 404,
/// as on `/api/allocations/detail`. An absent `depth` is invest-core's default.
pub async fn get_book(State(st): State<AppState>, jar: Cookies, Query(q): Query<BookQuery>) -> Result<Json<dto::BookSnapshot>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let book = st
		.grpc
		.get_book(&token, &service, q.depth.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "book unavailable"))?;
	Ok(Json(book.into()))
}

/// `GET /api/book/trades?service=&limit=` — the public tape, newest first: price, size,
/// the taker's side and the time. No order ids and no parties.
pub async fn list_trades(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let tape = st
		.grpc
		.list_trades(&token, &service, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "trades unavailable"))?;
	Ok(Json(tape.into()))
}

/// `GET /api/book/candles?service=&resolution=&from=&to=` — OHLCV buckets over
/// `[from, to)` in unix seconds; an absent `to` is now. Buckets with no trade are omitted.
pub async fn list_candles(State(st): State<AppState>, jar: Cookies, Query(q): Query<CandlesQuery>) -> Result<Json<dto::CandleList>, ApiError> {
	let service = required_service(q.service)?;
	let resolution = match q.resolution.as_deref().filter(|r| !r.is_empty()) {
		Some(r) if wire_resolution::is_known(r) => r.to_string(),
		Some(r) => return Err(ApiError::BadRequest(format!("unknown resolution '{r}' — expected one of {}", wire_resolution::ALL.join(", ")))),
		None => return Err(ApiError::BadRequest(format!("resolution is required — one of {}", wire_resolution::ALL.join(", ")))),
	};
	let token = require_token(&st, &jar).await?;
	let req = iv::ListCandlesRequest {
		service,
		resolution,
		from: q.from.unwrap_or(0),
		to: q.to.unwrap_or(0),
	};
	let candles = st.grpc.list_candles(&token, req).await.map_err(|s| ApiError::read(s, "candles unavailable"))?;
	Ok(Json(candles.into()))
}

/// `GET /api/book/policy?service=` — the allocation's trading terms: whether the book is
/// open, the taker fee, tick, lot and market slippage. Part of deciding whether to trade
/// at all, so not gated on holding a position.
pub async fn get_policy(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::BookPolicy>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let policy = st.grpc.get_book_policy(&token, &service).await.map_err(|s| ApiError::read(s, "book policy unavailable"))?;
	Ok(Json(policy.into()))
}

// ── the caller's orders ──────────────────────────────────────────────────────

/// `POST /api/book/orders` — CSRF-checked: place an order. Body
/// `{service, side, kind, tif?, price?, size, client_order_id}`. `tif` may be left out on
/// a market order (always `ioc`); a limit order has to say. `price` is empty for a market
/// order. Idempotent by `client_order_id`: a retry returns the order the id already names.
pub async fn place_order(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let side = vocabulary(&v, "side", &wire_side::ALL)?;
	let kind = vocabulary(&v, "kind", &wire_kind::ALL)?;
	let tif = match required(&v, "tif") {
		Some(tif) if wire_tif::is_known(&tif) => tif,
		Some(tif) => return Err(ApiError::BadRequest(format!("unknown tif '{tif}' — expected one of {}", wire_tif::ALL.join(", ")))),
		None => String::new(),
	};
	let (Some(size), Some(client_order_id)) = (required(&v, "size"), required(&v, "client_order_id")) else {
		return Err(ApiError::BadRequest("size and client_order_id are required".into()));
	};
	let token = require_token(&st, &jar).await?;
	let req = iv::PlaceOrderRequest {
		service,
		side,
		kind,
		tif,
		price: editable(&v, "price"),
		size,
		client_order_id,
	};
	Ok(Json(st.grpc.place_order(&token, req).await?.into()))
}

/// `POST /api/book/orders/cancel` — CSRF-checked: take the caller's resting order off the
/// book; its unspent escrow is released. Never refused for a frozen caller.
pub async fn cancel_order(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let Some(order_id) = required(&parse_body(&body), "order_id") else {
		return Err(ApiError::BadRequest("order_id is required".into()));
	};
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_order(&token, &order_id).await?.into()))
}

/// `GET /api/book/orders?service=` — the caller's resting orders, oldest first. An absent
/// `service` is every allocation.
pub async fn list_open_orders(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_open_orders(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "orders unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/orders/history?service=&limit=` — the caller's orders in every state,
/// newest first. An absent `service` is every allocation; an absent `limit` invest-core's page.
pub async fn list_order_history(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_order_history(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "order history unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/fills?service=&limit=` — the caller's own fills, newest first, each with
/// their side, their order and the fee they paid as taker.
pub async fn list_fills(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let fills = st
		.grpc
		.list_user_trades(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "fills unavailable"))?;
	Ok(Json(fills.into()))
}

#[cfg(test)]
mod book_route_tests {
	// `Status` is a large error type tonic mandates in handler signatures.
	#![allow(clippy::result_large_err)]

	use std::{
		net::SocketAddr,
		pin::Pin,
		sync::{Arc, Mutex},
	};

	use axum::{
		body::Body,
		http::{Request, StatusCode, header},
	};
	use invest_contracts::invest::v1::book_service_server::{BookService, BookServiceServer};
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::{TcpListener, TcpStream},
	};
	use tokio_stream::Stream;
	use tonic::{Code, Request as GrpcRequest, Response as GrpcResponse, Status, transport::Server};

	use super::*;
	use crate::routes::test_support::{CSRF, Harness, INVESTOR, MANAGER, ORIGIN, Signed, bearer_ok, request, serve_core};

	const SERVICE: &str = "quy-nhon";
	const ORDER_ID: &str = "3f1c1a52-4b7e-4d0a-9d6c-0f5e2b7a9c11";

	// ── the stub invest-core ───────────────────────────────────────────────────

	/// What the BFF actually forwarded upstream, so a test can assert on the request
	/// invest-core would have seen rather than only on the response the browser gets.
	#[derive(Default)]
	struct Seen {
		place: Option<iv::PlaceOrderRequest>,
		cancel: Option<iv::CancelOrderRequest>,
		open_orders: Option<iv::ListOpenOrdersRequest>,
		history: Option<iv::ListOrderHistoryRequest>,
		user_trades: Option<iv::ListUserTradesRequest>,
		book: Option<iv::GetBookRequest>,
		trades: Option<iv::ListTradesRequest>,
		candles: Option<iv::ListCandlesRequest>,
		watch: Option<iv::WatchBookRequest>,
		set_policy: Option<iv::SetBookPolicyRequest>,
	}

	#[derive(Clone, Default)]
	struct Hub {
		/// When set, every book RPC fails with this code (the upstream-refusal cases).
		fail_with: Option<Code>,
		seen: Arc<Mutex<Seen>>,
	}

	impl Hub {
		fn new() -> Self {
			Self::default()
		}

		fn failing(code: Code) -> Self {
			Self {
				fail_with: Some(code),
				..Self::default()
			}
		}

		/// invest-core accepts the signed-in user's access token and nothing else; a stub
		/// that took any bearer would let a handler forwarding the wrong one pass here.
		fn guard<T>(&self, request: &GrpcRequest<T>) -> Result<(), Status> {
			if !bearer_ok(request) {
				return Err(Status::unauthenticated("the book requires the session's access token"));
			}
			match self.fail_with {
				Some(code) => Err(Status::new(code, "upstream refused")),
				None => Ok(()),
			}
		}
	}

	fn stub_order(id: &str, state: &str) -> iv::Order {
		iv::Order {
			id: id.into(),
			service: SERVICE.into(),
			user_id: "bank-user-1".into(),
			side: "buy".into(),
			kind: "limit".into(),
			tif: "gtc".into(),
			price: "101.50".into(),
			size: "10.0000".into(),
			filled: "2.5000".into(),
			remaining: "7.5000".into(),
			avg_fill_price: "101.25".into(),
			fee_paid: "0.25".into(),
			state: state.into(),
			reject_reason: String::new(),
			cancel_reason: if state == "cancelled" { "user".into() } else { String::new() },
			client_order_id: "cli-1".into(),
			created_at: 1_750_000_000,
			updated_at: 1_750_000_100,
		}
	}

	fn stub_trade(own: bool) -> iv::Trade {
		iv::Trade {
			id: "trade-1".into(),
			service: SERVICE.into(),
			price: "101.25".into(),
			size: "2.5000".into(),
			taker_side: "sell".into(),
			executed_at: 1_750_000_100,
			user_side: if own { "buy".into() } else { String::new() },
			order_id: if own { ORDER_ID.into() } else { String::new() },
			fee: if own { "0.25".into() } else { String::new() },
		}
	}

	fn stub_snapshot(depth: u32) -> iv::BookSnapshot {
		iv::BookSnapshot {
			service: SERVICE.into(),
			revision: 42,
			bids: (0..depth.min(2))
				.map(|i| iv::BookLevel {
					price: format!("{}.00", 100 - i),
					size: "5.0000".into(),
					orders: 2,
				})
				.collect(),
			asks: vec![iv::BookLevel {
				price: "102.00".into(),
				size: "1.0000".into(),
				orders: 1,
			}],
			last_price: "101.25".into(),
			last_side: "sell".into(),
			mid: "101.00".into(),
			spread: "2.00".into(),
			nav: "99.80".into(),
			volume_24h: "12.5000".into(),
			change_24h: "-2.5".into(),
			as_of: 1_750_000_200,
		}
	}

	fn stub_policy(req: Option<&iv::SetBookPolicyRequest>) -> iv::BookPolicy {
		match req {
			Some(r) => iv::BookPolicy {
				service: r.service.clone(),
				book_open: r.book_open,
				taker_fee_bps: r.taker_fee_bps,
				price_tick: if r.price_tick.is_empty() { "0.01".into() } else { r.price_tick.clone() },
				lot_size: if r.lot_size.is_empty() { "0.0001".into() } else { r.lot_size.clone() },
				market_slippage_bps: r.market_slippage_bps,
				allow_unbacked_trading: r.allow_unbacked_trading,
				updated_at: 1_750_000_300,
			},
			None => iv::BookPolicy {
				service: SERVICE.into(),
				book_open: true,
				taker_fee_bps: 25,
				price_tick: "0.01".into(),
				lot_size: "0.0001".into(),
				market_slippage_bps: 100,
				allow_unbacked_trading: true,
				updated_at: 1_750_000_300,
			},
		}
	}

	#[tonic::async_trait]
	impl BookService for Hub {
		type WatchBookStream = Pin<Box<dyn Stream<Item = Result<iv::BookEvent, Status>> + Send + 'static>>;

		async fn place_order(&self, request: GrpcRequest<iv::PlaceOrderRequest>) -> Result<GrpcResponse<iv::Order>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().place = Some(req.clone());
			Ok(GrpcResponse::new(iv::Order {
				side: req.side,
				kind: req.kind,
				tif: req.tif,
				size: req.size,
				client_order_id: req.client_order_id,
				..stub_order(ORDER_ID, "open")
			}))
		}

		async fn cancel_order(&self, request: GrpcRequest<iv::CancelOrderRequest>) -> Result<GrpcResponse<iv::Order>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().cancel = Some(req.clone());
			Ok(GrpcResponse::new(stub_order(&req.order_id, "cancelled")))
		}

		async fn list_open_orders(&self, request: GrpcRequest<iv::ListOpenOrdersRequest>) -> Result<GrpcResponse<iv::OrderList>, Status> {
			self.guard(&request)?;
			self.seen.lock().unwrap().open_orders = Some(request.into_inner());
			Ok(GrpcResponse::new(iv::OrderList {
				orders: vec![stub_order(ORDER_ID, "partially_filled")],
			}))
		}

		async fn list_order_history(&self, request: GrpcRequest<iv::ListOrderHistoryRequest>) -> Result<GrpcResponse<iv::OrderList>, Status> {
			self.guard(&request)?;
			self.seen.lock().unwrap().history = Some(request.into_inner());
			Ok(GrpcResponse::new(iv::OrderList {
				orders: vec![stub_order(ORDER_ID, "filled")],
			}))
		}

		async fn list_user_trades(&self, request: GrpcRequest<iv::ListUserTradesRequest>) -> Result<GrpcResponse<iv::TradeList>, Status> {
			self.guard(&request)?;
			self.seen.lock().unwrap().user_trades = Some(request.into_inner());
			Ok(GrpcResponse::new(iv::TradeList { trades: vec![stub_trade(true)] }))
		}

		async fn get_book(&self, request: GrpcRequest<iv::GetBookRequest>) -> Result<GrpcResponse<iv::BookSnapshot>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().book = Some(req.clone());
			Ok(GrpcResponse::new(stub_snapshot(if req.depth == 0 { 20 } else { req.depth })))
		}

		async fn list_trades(&self, request: GrpcRequest<iv::ListTradesRequest>) -> Result<GrpcResponse<iv::TradeList>, Status> {
			self.guard(&request)?;
			self.seen.lock().unwrap().trades = Some(request.into_inner());
			Ok(GrpcResponse::new(iv::TradeList { trades: vec![stub_trade(false)] }))
		}

		async fn list_candles(&self, request: GrpcRequest<iv::ListCandlesRequest>) -> Result<GrpcResponse<iv::CandleList>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().candles = Some(req.clone());
			Ok(GrpcResponse::new(iv::CandleList {
				service: req.service,
				resolution: req.resolution,
				candles: vec![iv::Candle {
					time: 1_750_000_000,
					open: "100.00".into(),
					high: "102.00".into(),
					low: "99.50".into(),
					close: "101.25".into(),
					volume: "12.5000".into(),
				}],
			}))
		}

		async fn watch_book(&self, request: GrpcRequest<iv::WatchBookRequest>) -> Result<GrpcResponse<Self::WatchBookStream>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().watch = Some(req.clone());
			let frame = iv::BookEvent {
				snapshot: Some(stub_snapshot(req.depth)),
				trades: vec![stub_trade(false)],
				orders_revision: 7,
			};
			Ok(GrpcResponse::new(Box::pin(tokio_stream::iter(vec![Ok(frame)]))))
		}

		async fn get_book_policy(&self, request: GrpcRequest<iv::GetBookPolicyRequest>) -> Result<GrpcResponse<iv::BookPolicy>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(stub_policy(None)))
		}

		async fn set_book_policy(&self, request: GrpcRequest<iv::SetBookPolicyRequest>) -> Result<GrpcResponse<iv::BookPolicy>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().set_policy = Some(req.clone());
			Ok(GrpcResponse::new(stub_policy(Some(&req))))
		}
	}

	// ── harness ─────────────────────────────────────────────────────────────────

	struct Fixture {
		h: Harness,
		signed: Signed,
	}

	/// The real router over the stub invest-core, and a session holding `permissions`.
	async fn fixture(hub: Hub, permissions: &[&str]) -> Option<Fixture> {
		let core = serve_core(Server::builder().add_service(BookServiceServer::new(hub))).await;
		let h = Harness::new(Some(core)).await?;
		let signed = h.sign_in(permissions).await;
		Some(Fixture { h, signed })
	}

	/// A request carrying the session (and, for mutations, the matching CSRF pair) — what
	/// the browser actually sends.
	fn signed(f: &Fixture, method: &str, uri: &str, body: Option<&str>, csrf: bool) -> Request<Body> {
		request(method, uri, Some(&f.signed), body, csrf)
	}

	/// The router on a real listener. A websocket upgrade needs hyper's upgrade machinery,
	/// which `oneshot` does not supply — without it the extractor answers 426 before the
	/// handler's own gates run — so the socket tests go over TCP like a browser would.
	async fn listen(f: &Fixture) -> SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");
		let app = f.h.app.clone();
		tokio::spawn(async move {
			axum::serve(listener, app).await.expect("the BFF serves");
		});
		addr
	}

	/// A websocket handshake as a browser sends it: the upgrade headers, the cookie when
	/// signed in, and the page's `Origin` unless a cross-site page is being simulated.
	/// Answers the status the handshake got, and the connection — upgraded on a 101, with
	/// the first frame next on the wire.
	async fn handshake(addr: SocketAddr, uri: &str, cookie: Option<&str>, origin: Option<&str>) -> (u16, TcpStream) {
		let mut stream = TcpStream::connect(addr).await.expect("connect to the BFF");
		let mut request =
			format!("GET {uri} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n");
		if let Some(cookie) = cookie {
			request.push_str(&format!("Cookie: {cookie}\r\n"));
		}
		if let Some(origin) = origin {
			request.push_str(&format!("Origin: {origin}\r\n"));
		}
		request.push_str("\r\n");
		stream.write_all(request.as_bytes()).await.expect("send the handshake");

		// Byte by byte up to the end of the head, so nothing past it — the first frame, on
		// a 101 — is swallowed into a read buffer.
		let mut head = Vec::new();
		let mut byte = [0u8; 1];
		while !head.ends_with(b"\r\n\r\n") {
			assert_ne!(stream.read(&mut byte).await.expect("read the response"), 0, "the BFF hung up mid-head");
			head.push(byte[0]);
		}
		let status_line = String::from_utf8_lossy(&head);
		let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
		(status, stream)
	}

	/// The next frame off an upgraded connection: its opcode and payload. Server frames are
	/// unmasked, and a stub payload never reaches the eight-byte length form.
	async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
		let mut head = [0u8; 2];
		stream.read_exact(&mut head).await.expect("read a frame head");
		let len = match head[1] & 0x7f {
			126 => {
				let mut ext = [0u8; 2];
				stream.read_exact(&mut ext).await.expect("read the extended length");
				usize::from(u16::from_be_bytes(ext))
			}
			127 => panic!("a stub frame is never that large"),
			n => usize::from(n),
		};
		let mut payload = vec![0u8; len];
		stream.read_exact(&mut payload).await.expect("read the payload");
		(head[0] & 0x0f, payload)
	}

	const OPCODE_TEXT: u8 = 0x1;
	const OPCODE_CLOSE: u8 = 0x8;

	async fn send(f: &Fixture, request: Request<Body>) -> (StatusCode, Value) {
		let (status, body, _) = f.h.send(request).await;
		(status, body)
	}

	const PLACE: &str = r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"101.50","size":"10","client_order_id":"cli-1"}"#;

	// ── the gates ───────────────────────────────────────────────────────────────

	/// Every book route is behind the session cookie. Without one nothing is read and
	/// nothing is written.
	#[tokio::test]
	async fn no_session_reaches_nothing() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		for (method, uri, body) in [
			("GET", "/cabinet/api/book?service=quy-nhon", None),
			("GET", "/cabinet/api/book/trades?service=quy-nhon", None),
			("GET", "/cabinet/api/book/candles?service=quy-nhon&resolution=1h", None),
			("GET", "/cabinet/api/book/policy?service=quy-nhon", None),
			("GET", "/cabinet/api/book/orders", None),
			("GET", "/cabinet/api/book/orders/history", None),
			("GET", "/cabinet/api/book/fills", None),
			("POST", "/cabinet/api/book/orders", Some(PLACE)),
			("POST", "/cabinet/api/book/orders/cancel", Some(r#"{"order_id":"x"}"#)),
			(
				"POST",
				"/cabinet/api/admin/allocations/book",
				Some(r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#),
			),
		] {
			// The CSRF pair is present so the mutations get past the double-submit gate (which
			// runs first) and are refused on the SESSION, the thing this test is about.
			let mut builder = Request::builder()
				.method(method)
				.uri(uri)
				.header(header::COOKIE, format!("invest_csrf={CSRF}"))
				.header(header::ORIGIN, ORIGIN)
				.header("x-ev-csrf", CSRF);
			if body.is_some() {
				builder = builder.header(header::CONTENT_TYPE, "application/json");
			}
			let request = builder.body(body.map_or_else(Body::empty, |b: &str| Body::from(b.to_owned()))).unwrap();
			let (status, _) = send(&f, request).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} must refuse an unauthenticated caller");
		}
		let addr = listen(&f).await;
		let (status, _) = handshake(addr, "/cabinet/api/book/ws?service=quy-nhon", None, Some(ORIGIN)).await;
		assert_eq!(status, 401, "the socket must refuse an unauthenticated handshake");

		let seen = seen.lock().unwrap();
		assert!(seen.place.is_none() && seen.cancel.is_none() && seen.set_policy.is_none() && seen.watch.is_none());
	}

	/// The double-submit gate on the mutations. A valid session is not enough: a cross-site
	/// post carries the cookie but cannot read it to echo the header.
	#[tokio::test]
	async fn a_mutation_without_the_csrf_header_is_refused() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, MANAGER).await else { return };

		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders", Some(PLACE), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "placing must require the CSRF echo");
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders/cancel", Some(r#"{"order_id":"x"}"#), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "cancelling must require the CSRF echo");
		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#;
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "setting the policy must require the CSRF echo");

		let seen = seen.lock().unwrap();
		assert!(
			seen.place.is_none() && seen.cancel.is_none() && seen.set_policy.is_none(),
			"a CSRF failure must be decided before invest-core is called"
		);
	}

	/// The socket's half of CSRF: a handshake from any origin but the cabinet's is refused,
	/// with the cookie present and valid — before the feed is subscribed.
	#[tokio::test]
	async fn a_cross_site_handshake_is_refused() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };
		let addr = listen(&f).await;

		for origin in [None, Some("https://evil.test")] {
			let (status, _) = handshake(addr, "/cabinet/api/book/ws?service=quy-nhon", Some(&f.signed.cookie), origin).await;
			assert_eq!(status, 403, "origin {origin:?} must be refused");
		}
		let seen = seen.lock().unwrap();
		assert!(seen.watch.is_none(), "a refused handshake must never subscribe upstream");
	}

	/// The policy write is the operator's alone. A plain investor holds a perfectly valid
	/// session — the permission is what stops them, read from concierge's `GetMe`.
	#[tokio::test]
	async fn an_investor_cannot_set_the_policy() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#;
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert!(seen.lock().unwrap().set_policy.is_none(), "a refused caller must never reach invest-core");
	}

	// ── reading ─────────────────────────────────────────────────────────────────

	/// The snapshot the screen opens on. `revision` (uint64) and `as_of` (int64) cross as
	/// STRINGS, the levels keep their side order, and `depth` reaches invest-core as asked.
	#[tokio::test]
	async fn the_book_reaches_the_browser_intact() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book?service=quy-nhon&depth=5", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["revision"], "42", "revision must serialize as a string, not a number");
		assert_eq!(body["bids"][0]["price"], "100.00");
		assert_eq!(body["bids"][1]["price"], "99.00", "bids are best (highest) first");
		assert_eq!(body["bids"][0]["orders"], 2);
		assert_eq!(body["asks"][0]["price"], "102.00");
		assert_eq!(body["last_price"], "101.25");
		assert_eq!(body["last_side"], "sell");
		assert_eq!(body["mid"], "101.00");
		assert_eq!(body["spread"], "2.00");
		assert_eq!(body["nav"], "99.80");
		assert_eq!(body["volume_24h"], "12.5000");
		assert_eq!(body["change_24h"], "-2.5");
		assert_eq!(body["as_of"], "1750000200", "as_of must serialize as a string, not a number");

		let seen = seen.lock().unwrap();
		assert_eq!(seen.book.as_ref().map(|r| r.depth), Some(5));
	}

	/// The public tape discloses no party: the caller-only fields cross EMPTY, never
	/// dropped (the browser type has them) and never filled in.
	#[tokio::test]
	async fn the_public_tape_names_no_party() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book/trades?service=quy-nhon&limit=30", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let trade = &body["trades"][0];
		assert_eq!(trade["price"], "101.25");
		assert_eq!(trade["taker_side"], "sell");
		assert_eq!(trade["executed_at"], "1750000100");
		assert_eq!(trade["user_side"], "");
		assert_eq!(trade["order_id"], "");
		assert_eq!(trade["fee"], "");
		assert_eq!(seen.lock().unwrap().trades.as_ref().map(|r| r.limit), Some(30));
	}

	/// The caller's own tape carries their side, their order and the fee they paid.
	#[tokio::test]
	async fn own_fills_carry_the_side_the_order_and_the_fee() {
		let Some(f) = fixture(Hub::new(), INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book/fills?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let fill = &body["trades"][0];
		assert_eq!(fill["user_side"], "buy");
		assert_eq!(fill["order_id"], ORDER_ID);
		assert_eq!(fill["fee"], "0.25");
	}

	/// The three per-caller lists take an OPTIONAL allocation: absent means every one, and
	/// that is forwarded as the empty filter invest-core reads that way, never refused.
	#[tokio::test]
	async fn own_lists_without_an_allocation_mean_every_allocation() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		for uri in ["/cabinet/api/book/orders", "/cabinet/api/book/orders/history?limit=10", "/cabinet/api/book/fills"] {
			let (status, _) = send(&f, signed(&f, "GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::OK, "{uri}");
		}
		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book/orders?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let order = &body["orders"][0];
		assert_eq!(order["state"], "partially_filled");
		assert_eq!(order["remaining"], "7.5000");
		assert_eq!(order["created_at"], "1750000000", "created_at must serialize as a string, not a number");

		let seen = seen.lock().unwrap();
		assert_eq!(seen.open_orders.as_ref().map(|r| r.service.as_str()), Some(SERVICE), "the last call named the allocation");
		assert_eq!(seen.history.as_ref().map(|r| (r.service.as_str(), r.limit)), Some(("", 10)));
		assert_eq!(seen.user_trades.as_ref().map(|r| (r.service.as_str(), r.limit)), Some(("", 0)));
	}

	/// Every per-book read names its allocation in the query string, and a missing one is a
	/// client error decided here — never a call to invest-core with an empty service id.
	#[tokio::test]
	async fn a_per_book_read_without_an_allocation_is_rejected_locally() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		for uri in [
			"/cabinet/api/book",
			"/cabinet/api/book?service=",
			"/cabinet/api/book/trades?service=%20",
			"/cabinet/api/book/candles?resolution=1h",
			"/cabinet/api/book/policy",
		] {
			let (status, _) = send(&f, signed(&f, "GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} must be rejected before invest-core is called");
		}
		let addr = listen(&f).await;
		let (status, _) = handshake(addr, "/cabinet/api/book/ws", Some(&f.signed.cookie), Some(ORIGIN)).await;
		assert_eq!(status, 400, "the socket names one book; without it there is nothing to watch");
		let seen = seen.lock().unwrap();
		assert!(seen.book.is_none() && seen.trades.is_none() && seen.candles.is_none() && seen.watch.is_none());
	}

	/// Candles: the resolution is checked against the contract's words here, and the
	/// window crosses as given — an absent `to` is 0, which invest-core reads as now.
	#[tokio::test]
	async fn candles_check_the_resolution_and_forward_the_window() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		for uri in ["/cabinet/api/book/candles?service=quy-nhon", "/cabinet/api/book/candles?service=quy-nhon&resolution=2h"] {
			let (status, body) = send(&f, signed(&f, "GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
			assert!(body["error"].as_str().unwrap().contains("1m, 5m, 15m, 1h, 4h, 1d"), "the refusal names the vocabulary");
		}

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book/candles?service=quy-nhon&resolution=1h&from=1750000000", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["resolution"], "1h");
		let candle = &body["candles"][0];
		assert_eq!(candle["time"], "1750000000", "time must serialize as a string, not a number");
		assert_eq!(candle["close"], "101.25");
		assert_eq!(candle["volume"], "12.5000");

		let forwarded = seen.lock().unwrap().candles.clone().expect("invest-core saw the read");
		assert_eq!((forwarded.from, forwarded.to), (1_750_000_000, 0));
	}

	/// The terms an investor reads before trading.
	#[tokio::test]
	async fn the_policy_reaches_the_browser_intact() {
		let Some(f) = fixture(Hub::new(), INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book/policy?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["book_open"], true);
		assert_eq!(body["taker_fee_bps"], 25);
		assert_eq!(body["price_tick"], "0.01");
		assert_eq!(body["lot_size"], "0.0001");
		assert_eq!(body["market_slippage_bps"], 100);
		assert_eq!(body["allow_unbacked_trading"], true, "the terminal reads the acknowledgement to show its notice");
		assert_eq!(body["updated_at"], "1750000300");
	}

	/// A read that fails upstream maps invest-core's code to HTTP but never relays its wording:
	/// a hidden allocation is a 404 exactly as an unknown one, with the same fixed message.
	#[tokio::test]
	async fn a_failed_read_maps_the_code_and_hides_the_detail() {
		let Some(f) = fixture(Hub::failing(Code::NotFound), INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "GET", "/cabinet/api/book?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(body["error"], "book unavailable");
	}

	// ── writing ─────────────────────────────────────────────────────────────────

	/// Placing forwards the whole order as given — the tuple IS the order — and the
	/// invest-core's answer comes back with its 64-bit fields as strings.
	#[tokio::test]
	async fn placing_forwards_the_whole_order() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders", Some(PLACE), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["id"], ORDER_ID);
		assert_eq!(body["state"], "open");
		assert_eq!(body["client_order_id"], "cli-1");
		assert_eq!(body["updated_at"], "1750000100");
		assert_eq!(
			(body["reject_reason"].as_str(), body["cancel_reason"].as_str()),
			(Some(""), Some("")),
			"a resting order carries neither reason"
		);

		let forwarded = seen.lock().unwrap().place.clone().expect("invest-core saw the order");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!((forwarded.side.as_str(), forwarded.kind.as_str(), forwarded.tif.as_str()), ("buy", "limit", "gtc"));
		assert_eq!((forwarded.price.as_str(), forwarded.size.as_str()), ("101.50", "10"));
		assert_eq!(forwarded.client_order_id, "cli-1");
	}

	/// A market order may leave `tif` and `price` out: both cross EMPTY and invest-core fills
	/// in `ioc` and the derived limit. The BFF must not invent either.
	#[tokio::test]
	async fn a_market_order_leaves_tif_and_price_to_invest_core() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let body = r#"{"service":"quy-nhon","side":"sell","kind":"market","size":"3","client_order_id":"cli-2"}"#;
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);

		let forwarded = seen.lock().unwrap().place.clone().expect("invest-core saw the order");
		assert_eq!((forwarded.kind.as_str(), forwarded.tif.as_str(), forwarded.price.as_str()), ("market", "", ""));
	}

	/// A word outside the contract's vocabulary, or a missing required field, is refused
	/// here with the words that would have worked — before invest-core is called.
	#[tokio::test]
	async fn a_malformed_order_is_refused_before_it_reaches_invest_core() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		for (body, expect) in [
			(r#"{"side":"buy","kind":"limit","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#, "service"),
			(
				r#"{"service":"quy-nhon","side":"BUY","kind":"limit","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#,
				"buy, sell",
			),
			(
				r#"{"service":"quy-nhon","side":"buy","kind":"stop","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#,
				"limit, market",
			),
			(
				r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"fok","price":"1","size":"1","client_order_id":"c"}"#,
				"gtc, ioc, alo",
			),
			(r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"1","client_order_id":"c"}"#, "size"),
			(r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"1","size":"1"}"#, "client_order_id"),
		] {
			let (status, response) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
			assert!(response["error"].as_str().unwrap().contains(expect), "{body}: {}", response["error"]);
		}
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders/cancel", Some("{}"), true)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a cancel names its order");

		let seen = seen.lock().unwrap();
		assert!(seen.place.is_none() && seen.cancel.is_none(), "a malformed order must never reach invest-core");
	}

	/// A cancel forwards the id and answers with the order in its new state.
	#[tokio::test]
	async fn cancelling_forwards_the_order_id() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };

		let body = format!(r#"{{"order_id":"{ORDER_ID}"}}"#);
		let (status, response) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders/cancel", Some(&body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["id"], ORDER_ID);
		assert_eq!(response["state"], "cancelled");
		assert_eq!(response["cancel_reason"], "user", "invest-core's reason crosses verbatim");
		assert_eq!(seen.lock().unwrap().cancel.as_ref().map(|r| r.order_id.as_str()), Some(ORDER_ID));
	}

	/// A refusal on a mutation relays invest-core's client-safe wording under the mapped
	/// status: a closed book is a 412 the investor can read, not a fault.
	#[tokio::test]
	async fn a_refused_mutation_relays_invest_cores_wording() {
		let Some(f) = fixture(Hub::failing(Code::FailedPrecondition), INVESTOR).await else { return };

		let (status, body) = send(&f, signed(&f, "POST", "/cabinet/api/book/orders", Some(PLACE), true)).await;
		assert_eq!(status, StatusCode::PRECONDITION_FAILED);
		assert_eq!(body["error"], "upstream refused");
	}

	/// The operator's write forwards every term as given; an omitted tick or lot crosses
	/// empty (invest-core's defaults), an omitted slippage as zero.
	#[tokio::test]
	async fn setting_the_policy_forwards_the_whole_schedule() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, MANAGER).await else { return };

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":30,"price_tick":"0.05","lot_size":"0.01","market_slippage_bps":50}"#;
		let (status, response) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["book_open"], true);
		assert_eq!(response["taker_fee_bps"], 30);
		assert_eq!(response["price_tick"], "0.05");
		assert_eq!(response["updated_at"], "1750000300");

		let forwarded = seen.lock().unwrap().set_policy.clone().expect("invest-core saw the write");
		assert_eq!(forwarded.service, SERVICE);
		assert!(forwarded.book_open);
		assert_eq!((forwarded.taker_fee_bps, forwarded.market_slippage_bps), (30, 50));
		assert_eq!((forwarded.price_tick.as_str(), forwarded.lot_size.as_str()), ("0.05", "0.01"));

		let body = r#"{"service":"quy-nhon","book_open":false,"taker_fee_bps":0}"#;
		let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK, "closing a book with the default tick and lot is a whole schedule");
		let forwarded = seen.lock().unwrap().set_policy.clone().expect("invest-core saw the write");
		assert!(!forwarded.book_open);
		assert_eq!((forwarded.taker_fee_bps, forwarded.market_slippage_bps), (0, 0));
		assert_eq!((forwarded.price_tick.as_str(), forwarded.lot_size.as_str()), ("", ""));
	}

	/// The unbacked-trading acknowledgement crosses as given and comes back on the policy;
	/// left out, it crosses as `false` — invest-core, not the BFF, decides whether the product
	/// needed it.
	#[tokio::test]
	async fn the_unbacked_trading_acknowledgement_is_forwarded_and_echoed() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, MANAGER).await else { return };

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":0,"allow_unbacked_trading":true}"#;
		let (status, response) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["allow_unbacked_trading"], true);
		assert!(seen.lock().unwrap().set_policy.clone().expect("invest-core saw the write").allow_unbacked_trading);

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":0}"#;
		let (status, response) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["allow_unbacked_trading"], false);
		assert!(!seen.lock().unwrap().set_policy.clone().expect("invest-core saw the write").allow_unbacked_trading);
	}

	/// `book_open` and the taker fee are read as a whole: a missing or malformed one must
	/// fail loudly here rather than close a book, or price it at nothing, while the console
	/// answers "saved".
	#[tokio::test]
	async fn a_partial_policy_is_refused_before_it_reaches_invest_core() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, MANAGER).await else { return };

		for body in [
			r#"{"book_open":true,"taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","book_open":"yes","taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","book_open":true}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":"25"}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":-1}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25,"market_slippage_bps":2.5}"#,
		] {
			let (status, _) = send(&f, signed(&f, "POST", "/cabinet/api/admin/allocations/book", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "an incomplete policy must be refused: {body}");
		}
		assert!(seen.lock().unwrap().set_policy.is_none(), "an incomplete policy must never reach invest-core");
	}

	// ── the socket ──────────────────────────────────────────────────────────────

	/// The whole bridge, end to end: a signed-in, same-origin handshake naming a book is
	/// upgraded, the subscription it opens upstream carries the session's token and the depth
	/// asked for, the first frame is the book as the REST read would answer it, and when the
	/// invest-core's stream ends the socket is closed normally so the client reconnects.
	#[tokio::test]
	async fn a_same_origin_handshake_is_upgraded_and_streams_the_book() {
		let hub = Hub::new();
		let seen = hub.seen.clone();
		let Some(f) = fixture(hub, INVESTOR).await else { return };
		let addr = listen(&f).await;

		let (status, mut socket) = handshake(addr, "/cabinet/api/book/ws?service=quy-nhon&depth=10", Some(&f.signed.cookie), Some(ORIGIN)).await;
		assert_eq!(status, 101);

		let (opcode, payload) = read_frame(&mut socket).await;
		assert_eq!(opcode, OPCODE_TEXT);
		let frame: Value = serde_json::from_slice(&payload).expect("a frame is JSON");
		assert_eq!(frame["type"], "book");
		assert_eq!(frame["snapshot"]["service"], SERVICE);
		assert_eq!(frame["snapshot"]["revision"], "42", "the revision travels as a string");
		assert_eq!(frame["snapshot"]["bids"].as_array().map(Vec::len), Some(2));
		assert_eq!(frame["snapshot"]["nav"], "99.80");
		assert_eq!(frame["trades"][0]["user_side"], "", "the socket's tape names no party");
		assert_eq!(frame["orders_revision"], "7");

		let (opcode, payload) = read_frame(&mut socket).await;
		assert_eq!(opcode, OPCODE_CLOSE, "an ended upstream closes the socket");
		assert_eq!(
			u16::from_be_bytes([payload[0], payload[1]]),
			1000,
			"…normally, so the client reconnects rather than signs in again"
		);

		let seen = seen.lock().unwrap();
		let watched = seen.watch.as_ref().expect("invest-core saw the subscription, with the session's token");
		assert_eq!((watched.service.as_str(), watched.depth), (SERVICE, 10));
	}
}
