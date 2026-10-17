//! `book` context — holders trading an allocation's units with each other.
//!
//! Placing acts on the caller's own `sub`; the use case refuses a suspended caller, and
//! banking refuses a buy's hold for a frozen one. Cancelling is behind `caller_id` only,
//! like cancelling a redemption — a suspended user may still unwind. Every read resolves
//! the allocation as the caller sees it, so the book of a product hidden from them is
//! `NOT_FOUND` exactly as the product is. The policy is an operator's
//! (`invest:allocation:manage`), the same seam that opens a product.
//!
//! `WatchBook` is the one server stream invest-core serves. It is a `watch` receiver turned
//! into a stream by hand ([`BookWatch`]): every frame is built by re-reading the book,
//! so a subscriber that lags sees the latest state, never a backlog. Authorized at the
//! handshake, it is then bounded: a caller holds a capped number of streams at once, and a
//! live stream re-checks its holder on a timer and ends when they are suspended, lose sight
//! of the book, or their token expires.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use std::{
	future::Future,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
	time::Duration,
};

use async_trait::async_trait;
use evconcierge_auth::claims_of;
use invest_contracts::invest::v1::{self as pb, book_service_server::BookService};
use invest_domain::{
	authz::Permission,
	balance::ServiceId,
	book::{BookPolicy, CancelReason, CandleResolution, ClientOrderId, OrderId, OrderKind, Price, Side, Tif},
	money::Shares,
	users::UserId,
};
use tokio::{
	sync::watch,
	time::{Instant, Interval, MissedTickBehavior},
};
use tonic::{Request, Response, Status, codegen::tokio_stream::Stream};
use uuid::Uuid;

use crate::{
	AppState,
	application::book::{self as book_app, BookPorts, BookSnapshotView, PlaceOrderRequest, StreamSlot, WatchFrame, WatchStreams},
	ports::{
		book::{BookLevel, BookPolicyRecord, Candle, OrderRecord, TradeRecord, UserTrade},
		users::UserStanding,
	},
	services::support::{active_caller, caller_id, holds_permission, map_err, optional, require_permission, unix_now},
};

const DEFAULT_LIST_LIMIT: u32 = 100;
const MAX_LIST_LIMIT: u32 = 200;
const DEFAULT_TAPE_LIMIT: u32 = 50;
const DEFAULT_DEPTH: u32 = 20;
const MAX_DEPTH: u32 = 50;
/// How many public trades ride on each `WatchBook` frame.
const WATCH_TRADES: u32 = 20;

#[derive(Clone)]
pub struct BookSvc {
	pub state: AppState,
}

impl BookSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	fn ports(&self) -> BookPorts<'_> {
		BookPorts {
			allocations: self.state.allocations.as_ref(),
			ledger: self.state.ledger.as_ref(),
			money: self.state.money.as_ref(),
			nav: self.state.nav.as_ref(),
			store: self.state.book.as_ref(),
			engine: self.state.book_engine.as_ref(),
			users: self.state.users.as_ref(),
			relay: &self.state.relay_notify,
			feed: &self.state.book_feed,
		}
	}

	/// The visibility gate every book read runs: the allocation as the caller sees it,
	/// widened for an `AllocationManage` holder.
	async fn visible<T>(&self, request: &Request<T>, service: &ServiceId) -> Result<UserId, Status> {
		let caller = caller_id(request)?;
		let unrestricted = holds_permission(&self.state, request, Permission::AllocationManage).await?;
		book_app::require_visible(self.state.allocations.as_ref(), service, caller, unrestricted).await.map_err(map_err)?;
		Ok(caller)
	}
}

#[tonic::async_trait]
impl BookService for BookSvc {
	type WatchBookStream = Pin<Box<dyn Stream<Item = Result<pb::BookEvent, Status>> + Send + 'static>>;

	async fn place_order(&self, request: Request<pb::PlaceOrderRequest>) -> Result<Response<pb::Order>, Status> {
		// An order escrows money: a caller invest does not know, or has suspended, is refused
		// before anything is read or held.
		let user = active_caller(&self.state, &request).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let side = Side::parse(&req.side).map_err(map_err)?;
		let kind = OrderKind::parse(&req.kind).map_err(map_err)?;
		// A market order is always immediate-or-cancel, so an unstated time in force on
		// one is that rather than a client bug; a limit order has to say.
		let tif = match (kind, optional(&req.tif)) {
			(OrderKind::Market, None) => Tif::Ioc,
			(_, raw) => Tif::parse(raw.unwrap_or_default()).map_err(map_err)?,
		};
		// Parsed at the boundary, so a malformed figure is an `invalid_argument` about the
		// input rather than a validation error from inside the domain.
		let price = optional(&req.price).map(Price::parse_decimal).transpose().map_err(map_err)?;
		let size = Shares::parse_decimal(&req.size).map_err(map_err)?;
		let client_order_id = ClientOrderId::parse(&req.client_order_id).map_err(map_err)?;
		let record = book_app::place_order(
			&self.ports(),
			user,
			PlaceOrderRequest {
				service,
				side,
				kind,
				tif,
				price,
				size,
				client_order_id,
			},
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(order_to_proto(&record, true)))
	}

	async fn cancel_order(&self, request: Request<pb::CancelOrderRequest>) -> Result<Response<pb::Order>, Status> {
		let user = caller_id(&request)?;
		let id = parse_order_id(&request.get_ref().order_id)?;
		let record = book_app::cancel_order(self.state.book.as_ref(), &self.state.relay_notify, &self.state.book_feed, id, user)
			.await
			.map_err(map_err)?;
		Ok(Response::new(order_to_proto(&record, true)))
	}

	async fn list_open_orders(&self, request: Request<pb::ListOpenOrdersRequest>) -> Result<Response<pb::OrderList>, Status> {
		let user = caller_id(&request)?;
		let service = parse_service_filter(&request.get_ref().service)?;
		let records = book_app::list_open_orders(self.state.book.as_ref(), user, service.as_ref()).await.map_err(map_err)?;
		Ok(Response::new(pb::OrderList {
			orders: records.iter().map(|record| order_to_proto(record, true)).collect(),
		}))
	}

	async fn list_order_history(&self, request: Request<pb::ListOrderHistoryRequest>) -> Result<Response<pb::OrderList>, Status> {
		let user = caller_id(&request)?;
		let req = request.get_ref();
		let service = parse_service_filter(&req.service)?;
		let limit = bounded(req.limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT);
		let records = book_app::list_order_history(self.state.book.as_ref(), user, service.as_ref(), limit).await.map_err(map_err)?;
		Ok(Response::new(pb::OrderList {
			orders: records.iter().map(|record| order_to_proto(record, true)).collect(),
		}))
	}

	async fn list_user_trades(&self, request: Request<pb::ListUserTradesRequest>) -> Result<Response<pb::TradeList>, Status> {
		let user = caller_id(&request)?;
		let req = request.get_ref();
		let service = parse_service_filter(&req.service)?;
		let limit = bounded(req.limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT);
		let trades = book_app::list_user_trades(self.state.book.as_ref(), user, service.as_ref(), limit).await.map_err(map_err)?;
		Ok(Response::new(pb::TradeList {
			trades: trades.iter().map(user_trade_to_proto).collect(),
		}))
	}

	async fn get_book(&self, request: Request<pb::GetBookRequest>) -> Result<Response<pb::BookSnapshot>, Status> {
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		self.visible(&request, &service).await?;
		let depth = bounded(request.get_ref().depth, DEFAULT_DEPTH, MAX_DEPTH);
		let view = book_app::snapshot(
			self.state.book.as_ref(),
			self.state.nav.as_ref(),
			self.state.ledger.as_ref(),
			self.state.money.as_ref(),
			&service,
			depth,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(snapshot_to_proto(&view)))
	}

	async fn list_trades(&self, request: Request<pb::ListTradesRequest>) -> Result<Response<pb::TradeList>, Status> {
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		self.visible(&request, &service).await?;
		let limit = bounded(request.get_ref().limit, DEFAULT_TAPE_LIMIT, MAX_LIST_LIMIT);
		let trades = book_app::list_trades(self.state.book.as_ref(), &service, limit).await.map_err(map_err)?;
		Ok(Response::new(pb::TradeList {
			trades: trades.iter().map(public_trade_to_proto).collect(),
		}))
	}

	async fn list_candles(&self, request: Request<pb::ListCandlesRequest>) -> Result<Response<pb::CandleList>, Status> {
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		self.visible(&request, &service).await?;
		let req = request.get_ref();
		let resolution = CandleResolution::parse(&req.resolution).map_err(map_err)?;
		let to = if req.to == 0 { unix_now() } else { req.to };
		let candles = book_app::candles(self.state.book.as_ref(), &service, resolution, req.from, to).await.map_err(map_err)?;
		Ok(Response::new(pb::CandleList {
			service: service.to_string(),
			resolution: resolution.as_str().to_owned(),
			candles: candles.iter().map(candle_to_proto).collect(),
		}))
	}

	async fn watch_book(&self, request: Request<pb::WatchBookRequest>) -> Result<Response<Self::WatchBookStream>, Status> {
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let caller = self.visible(&request, &service).await?;
		// Taken after the gate, so a caller who may not watch is told that rather than
		// "too many", and a refused handshake never holds a slot.
		let slot = open_slot(&self.state.watch_streams, caller)?;
		// Read from the claims the layer already verified; the stream ends when it passes
		// because the token is the only thing that vouched for the caller.
		let claims = claims_of(&request);
		let expires_in = claims.as_ref().map(|claims| Duration::from_secs(claims.exp.saturating_sub(unix_now().max(0) as u64)));
		// The auth layer compared it once; the stream compares it again on every recheck.
		let token_version = claims.as_ref().map_or(0, |claims| claims.token_version);
		let depth = bounded(request.get_ref().depth, DEFAULT_DEPTH, MAX_DEPTH);
		let receiver = self.state.book_feed.subscribe(&service);
		let mut ticker = tokio::time::interval_at(Instant::now() + self.state.watch_streams.recheck_every(), self.state.watch_streams.recheck_every());
		// A slow recheck must not be followed by a burst of catch-up ones.
		ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
		Ok(Response::new(Box::pin(BookWatch::new(WatchState {
			backend: LiveWatch {
				state: self.state.clone(),
				service,
				caller,
				depth,
				token_version,
			},
			receiver,
			ticker,
			deadline: expires_in.map(|left| Instant::now() + left),
			slot: Some(slot),
			finished: false,
		}))))
	}

	async fn get_book_policy(&self, request: Request<pb::GetBookPolicyRequest>) -> Result<Response<pb::BookPolicy>, Status> {
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		self.visible(&request, &service).await?;
		let record = book_app::policy(self.state.book.as_ref(), &service).await.map_err(map_err)?;
		Ok(Response::new(policy_to_proto(&record)))
	}

	async fn set_book_policy(&self, request: Request<pb::SetBookPolicyRequest>) -> Result<Response<pb::BookPolicy>, Status> {
		require_permission(&self.state, &request, Permission::AllocationManage).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let price_tick = optional(&req.price_tick)
			.map(Price::parse_decimal)
			.transpose()
			.map_err(map_err)?
			.unwrap_or(BookPolicy::DEFAULT_PRICE_TICK);
		let lot_size = optional(&req.lot_size)
			.map(Shares::parse_decimal)
			.transpose()
			.map_err(map_err)?
			.unwrap_or(BookPolicy::DEFAULT_LOT_SIZE);
		let policy = BookPolicy::new(req.book_open, req.taker_fee_bps, price_tick, lot_size, req.market_slippage_bps, req.allow_unbacked_trading).map_err(map_err)?;
		let record = book_app::set_policy(self.state.allocations.as_ref(), self.state.book.as_ref(), &service, policy)
			.await
			.map_err(map_err)?;
		Ok(Response::new(policy_to_proto(&record)))
	}
}

fn open_slot(streams: &Arc<WatchStreams>, caller: UserId) -> Result<StreamSlot, Status> {
	streams
		.acquire(caller)
		.ok_or_else(|| Status::resource_exhausted("too many open book streams — close one and retry"))
}

/// What a `WatchBook` stream asks of the world; the stream itself only schedules.
#[async_trait]
trait WatchBackend: Send + Sync + Unpin + 'static {
	/// The next frame, built by re-reading the book.
	async fn frame(&self) -> Result<pb::BookEvent, Status>;
	/// Whether the holder may still watch; `Err` closes the stream with that status.
	async fn recheck(&self) -> Result<(), Status>;
}

/// The production backend: invest-core's own ports.
struct LiveWatch {
	state: AppState,
	service: ServiceId,
	caller: UserId,
	depth: u32,
	/// The `token_version` claim of the token that opened the stream.
	token_version: u64,
}

#[async_trait]
impl WatchBackend for LiveWatch {
	async fn frame(&self) -> Result<pb::BookEvent, Status> {
		book_app::watch_frame(
			self.state.book.as_ref(),
			self.state.nav.as_ref(),
			self.state.ledger.as_ref(),
			self.state.money.as_ref(),
			&self.service,
			self.caller,
			self.depth,
			WATCH_TRADES,
			unix_now(),
		)
		.await
		.map(|frame| frame_to_proto(&frame))
		.map_err(map_err)
	}

	/// The handshake gate again: the token's revocation floor, the projection's standing,
	/// then the book's visibility as that standing now allows it.
	async fn recheck(&self) -> Result<(), Status> {
		let threshold = self.state.users.token_version(self.caller).await.map_err(|_| Status::unavailable("internal error"))?;
		not_revoked(self.token_version, threshold)?;
		let standing = self.state.users.standing(self.caller).await.map_err(|_| Status::unavailable("internal error"))?;
		let unrestricted = still_entitled(standing.as_ref())?;
		book_app::require_visible(self.state.allocations.as_ref(), &self.service, self.caller, unrestricted)
			.await
			.map_err(map_err)
	}
}

/// The auth layer's revocation rule, for a token that outlives its request: below the
/// threshold is revoked; no threshold seen means nothing to compare, so it stands.
fn not_revoked(claim: u64, threshold: Option<u64>) -> Result<(), Status> {
	match threshold {
		Some(threshold) if claim < threshold => Err(Status::unauthenticated("token revoked")),
		_ => Ok(()),
	}
}

/// Whether a live stream's holder keeps their place, and whether they see past `hidden`.
/// An unknown holder is not refused here (the handshake never required one: any invest
/// access level is enough), but a suspended one is, as `holds_permission` does.
fn still_entitled(standing: Option<&UserStanding>) -> Result<bool, Status> {
	match standing {
		None => Ok(false),
		Some(standing) if !standing.active => Err(Status::permission_denied("account is suspended")),
		Some(standing) => Ok(standing.may(Permission::AllocationManage)),
	}
}

/// What one `WatchBook` subscription carries between frames.
struct WatchState<B> {
	backend: B,
	receiver: watch::Receiver<u64>,
	/// Paces the holder recheck.
	ticker: Interval,
	/// When the token that vouched for the caller expires, if known.
	deadline: Option<Instant>,
	/// Held for the stream's life; `Drop` frees it, including when the client cancels.
	slot: Option<StreamSlot>,
	/// Set once a closing status has been emitted: the stream yields nothing after it.
	finished: bool,
}

type FrameFuture<B> = Pin<Box<dyn Future<Output = Option<(Result<pb::BookEvent, Status>, WatchState<B>)>> + Send>>;

/// A `watch` receiver as a tonic stream: wait for the book to move, frame it, repeat —
/// and meanwhile recheck the holder on a timer and watch the token's clock.
/// Hand-rolled rather than `tokio_stream::wrappers::WatchStream` because the tonic
/// re-export of `tokio_stream` carries no `sync` feature and the workspace adds no
/// dependency for one wrapper. The state travels through the in-flight future and back,
/// so the stream holds it exactly once at any time.
struct BookWatch<B> {
	state: Option<WatchState<B>>,
	pending: Option<FrameFuture<B>>,
}

impl<B: WatchBackend> BookWatch<B> {
	fn new(state: WatchState<B>) -> Self {
		Self { state: Some(state), pending: None }
	}
}

/// Close the stream with `status`: the slot is given back at once rather than whenever
/// the transport gets around to dropping the stream.
fn close<B>(mut watch: WatchState<B>, status: Status) -> Option<(Result<pb::BookEvent, Status>, WatchState<B>)> {
	watch.finished = true;
	watch.slot = None;
	Some((Err(status), watch))
}

/// Wait for a change, then frame — or close, when the recheck or the token says so.
/// `None` only if the feed itself is gone (the sender lives in `AppState`, so in practice
/// the stream ends when the client hangs up).
async fn next_frame<B: WatchBackend>(mut watch: WatchState<B>) -> Option<(Result<pb::BookEvent, Status>, WatchState<B>)> {
	loop {
		let deadline = watch.deadline;
		let expiry = async {
			match deadline {
				Some(at) => tokio::time::sleep_until(at).await,
				None => std::future::pending().await,
			}
		};
		tokio::select! {
			changed = watch.receiver.changed() => {
				changed.ok()?;
				// Only the fact that it moved matters — the frame re-reads the book, so the
				// revision it carries is at least this one and never behind it.
				watch.receiver.borrow_and_update();
				let frame = watch.backend.frame().await;
				return Some((frame, watch));
			}
			_ = watch.ticker.tick() => match watch.backend.recheck().await {
				Ok(()) => {}
				// A blip in the projection must not drop every stream at once and send them
				// all reconnecting; the next tick asks again.
				Err(status) if status.code() == tonic::Code::Unavailable => {
					tracing::warn!("WatchBook recheck could not read the holder, keeping the stream: {}", status.message());
				}
				Err(status) => return close(watch, status),
			},
			() = expiry => return close(watch, Status::unauthenticated("access token expired")),
		}
	}
}

impl<B: WatchBackend> Stream for BookWatch<B> {
	type Item = Result<pb::BookEvent, Status>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		loop {
			if let Some(pending) = this.pending.as_mut() {
				return match pending.as_mut().poll(cx) {
					Poll::Ready(Some((item, state))) => {
						this.pending = None;
						this.state = Some(state);
						Poll::Ready(Some(item))
					}
					Poll::Ready(None) => {
						this.pending = None;
						Poll::Ready(None)
					}
					Poll::Pending => Poll::Pending,
				};
			}
			let Some(state) = this.state.take() else {
				return Poll::Ready(None);
			};
			if state.finished {
				return Poll::Ready(None);
			}
			this.pending = Some(Box::pin(next_frame(state)));
		}
	}
}

fn parse_order_id(raw: &str) -> Result<OrderId, Status> {
	Uuid::parse_str(raw).map(OrderId::from_raw).map_err(|_| Status::invalid_argument("invalid order id"))
}

fn parse_service_filter(raw: &str) -> Result<Option<ServiceId>, Status> {
	optional(raw).map(ServiceId::parse).transpose().map_err(map_err)
}

/// A `0` on the wire is "the server default"; anything above the cap is the cap.
fn bounded(requested: u32, default: u32, max: u32) -> u32 {
	if requested == 0 { default } else { requested.min(max) }
}

/// `own` decides whether `user_id` is disclosed — only ever on the caller's own orders.
fn order_to_proto(record: &OrderRecord, own: bool) -> pb::Order {
	let order = &record.order;
	pb::Order {
		id: order.id().to_string(),
		service: order.service().to_string(),
		user_id: if own { order.user().to_string() } else { String::new() },
		side: order.side().as_str().to_owned(),
		kind: order.kind().as_str().to_owned(),
		tif: order.tif().as_str().to_owned(),
		price: order.price().to_decimal_string(),
		size: order.size().to_decimal_string(),
		filled: order.filled().to_decimal_string(),
		remaining: order.remaining().to_decimal_string(),
		avg_fill_price: order.average_fill_price().map(|price| price.to_decimal_string()).unwrap_or_default(),
		fee_paid: order.fee_paid().to_decimal_string(),
		state: order.state().as_str().to_owned(),
		reject_reason: order.reject_reason().unwrap_or_default().to_owned(),
		cancel_reason: order.cancel_reason().map(CancelReason::as_str).unwrap_or_default().to_owned(),
		client_order_id: order.client_order_id().as_str().to_owned(),
		created_at: record.created_at,
		updated_at: record.updated_at,
	}
}

/// The public tape row: no parties, no order ids.
fn public_trade_to_proto(record: &TradeRecord) -> pb::Trade {
	let trade = &record.trade;
	pb::Trade {
		id: trade.id.to_string(),
		service: trade.service.to_string(),
		price: trade.price.to_decimal_string(),
		size: trade.size.to_decimal_string(),
		taker_side: trade.taker_side.as_str().to_owned(),
		executed_at: record.executed_at,
		user_side: String::new(),
		order_id: String::new(),
		fee: String::new(),
	}
}

/// The caller's own view of a fill: their side, their order, their fee.
fn user_trade_to_proto(view: &UserTrade) -> pb::Trade {
	pb::Trade {
		user_side: view.side.as_str().to_owned(),
		order_id: view.order_id.to_string(),
		fee: view.fee.to_decimal_string(),
		..public_trade_to_proto(&view.trade)
	}
}

fn level_to_proto(level: &BookLevel) -> pb::BookLevel {
	pb::BookLevel {
		price: level.price.to_decimal_string(),
		size: level.size.to_decimal_string(),
		orders: level.orders,
	}
}

fn snapshot_to_proto(view: &BookSnapshotView) -> pb::BookSnapshot {
	pb::BookSnapshot {
		service: view.service.to_string(),
		revision: view.revision,
		bids: view.bids.iter().map(level_to_proto).collect(),
		asks: view.asks.iter().map(level_to_proto).collect(),
		last_price: view.last.map(|(price, _)| price.to_decimal_string()).unwrap_or_default(),
		last_side: view.last.map(|(_, side)| side.as_str().to_owned()).unwrap_or_default(),
		mid: view.mid.map(|price| price.to_decimal_string()).unwrap_or_default(),
		spread: view.spread.map(|price| price.to_decimal_string()).unwrap_or_default(),
		nav: view.nav.to_decimal_string(),
		volume_24h: view.volume_24h.to_decimal_string(),
		change_24h: view.change_24h_pct.clone().unwrap_or_default(),
		as_of: view.as_of,
	}
}

fn frame_to_proto(frame: &WatchFrame) -> pb::BookEvent {
	pb::BookEvent {
		snapshot: Some(snapshot_to_proto(&frame.snapshot)),
		trades: frame.trades.iter().map(public_trade_to_proto).collect(),
		orders_revision: frame.orders_revision,
	}
}

fn candle_to_proto(candle: &Candle) -> pb::Candle {
	pb::Candle {
		time: candle.time,
		open: candle.open.to_decimal_string(),
		high: candle.high.to_decimal_string(),
		low: candle.low.to_decimal_string(),
		close: candle.close.to_decimal_string(),
		volume: candle.volume.to_decimal_string(),
	}
}

fn policy_to_proto(record: &BookPolicyRecord) -> pb::BookPolicy {
	let policy = &record.policy;
	pb::BookPolicy {
		service: record.service.to_string(),
		book_open: policy.book_open(),
		taker_fee_bps: policy.taker_fee_bps(),
		price_tick: policy.price_tick().to_decimal_string(),
		lot_size: policy.lot_size().to_decimal_string(),
		market_slippage_bps: policy.market_slippage_bps(),
		updated_at: record.updated_at,
		allow_unbacked_trading: policy.allow_unbacked_trading(),
	}
}

/// The wire vocabulary in `evbanking_contracts::book` is what consumer repos match on;
/// the domain enums are what the hub stores. Drift between them is a test failure here.
#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use invest_contracts::book::{cancel_reason as wire_cancel_reason, kind as wire_kind, resolution as wire_resolution, side as wire_side, state as wire_state, tif as wire_tif};
	use invest_domain::book::OrderState;
	use tonic::{Code, codegen::tokio_stream::StreamExt};

	use super::*;

	#[test]
	fn domain_book_vocabularies_match_the_wire_contract() {
		assert_eq!([Side::Buy.as_str(), Side::Sell.as_str()], wire_side::ALL);
		assert_eq!([OrderKind::Limit.as_str(), OrderKind::Market.as_str()], wire_kind::ALL);
		assert_eq!([Tif::Gtc.as_str(), Tif::Ioc.as_str(), Tif::Alo.as_str()], wire_tif::ALL);
		let states = [OrderState::Open, OrderState::PartiallyFilled, OrderState::Filled, OrderState::Cancelled, OrderState::Rejected];
		assert_eq!(states.map(OrderState::as_str), wire_state::ALL);
		for state in states {
			assert_eq!(state.is_resting(), wire_state::is_resting(state.as_str()), "{state:?}");
		}
		assert_eq!(CandleResolution::ALL.map(CandleResolution::as_str), wire_resolution::ALL);
		assert_eq!(
			[CancelReason::User, CancelReason::IocRemainder, CancelReason::MarketRemainder].map(CancelReason::as_str),
			wire_cancel_reason::ALL
		);
	}

	#[test]
	fn list_limits_default_and_cap() {
		assert_eq!(bounded(0, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), DEFAULT_LIST_LIMIT);
		assert_eq!(bounded(7, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), 7);
		assert_eq!(bounded(10_000, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), MAX_LIST_LIMIT);
	}

	#[test]
	fn a_suspended_holder_is_refused_and_an_unknown_one_is_not_widened() {
		let standing = |active, permissions: Option<&[&str]>| UserStanding {
			active,
			permissions: permissions.map(|p| p.iter().map(|s| (*s).to_owned()).collect()),
		};
		assert_eq!(still_entitled(Some(&standing(false, None))).unwrap_err().code(), Code::PermissionDenied);
		assert!(!still_entitled(None).unwrap());
		assert!(!still_entitled(Some(&standing(true, Some(&[])))).unwrap());
		assert!(still_entitled(Some(&standing(true, Some(&["invest:allocation:manage"])))).unwrap());
	}

	#[test]
	fn a_caller_past_the_cap_is_resource_exhausted_and_a_freed_slot_is_reusable() {
		let streams = WatchStreams::new(2, Duration::from_secs(60)).unwrap();
		let (a, b) = (UserId::new(), UserId::new());
		let first = open_slot(&streams, a).unwrap();
		let _second = open_slot(&streams, a).unwrap();
		assert_eq!(open_slot(&streams, a).unwrap_err().code(), Code::ResourceExhausted);
		// The cap is per caller: another user is not counted against `a`.
		assert!(open_slot(&streams, b).is_ok());
		drop(first);
		assert!(open_slot(&streams, a).is_ok(), "a closed stream gives its slot back");
	}

	/// Stands in for the ports: the holder's standing is a switch the test flips.
	struct Fake {
		standing: Arc<Mutex<UserStanding>>,
		threshold: Arc<Mutex<Option<u64>>>,
	}

	/// The token the fake streams were opened with.
	const CLAIM: u64 = 3;

	#[async_trait]
	impl WatchBackend for Fake {
		async fn frame(&self) -> Result<pb::BookEvent, Status> {
			Ok(pb::BookEvent::default())
		}

		async fn recheck(&self) -> Result<(), Status> {
			not_revoked(CLAIM, *self.threshold.lock().unwrap())?;
			let standing = self.standing.lock().unwrap().clone();
			still_entitled(Some(&standing)).map(|_| ())
		}
	}

	const PERIOD: Duration = Duration::from_secs(60);

	fn standing(active: bool) -> Arc<Mutex<UserStanding>> {
		Arc::new(Mutex::new(UserStanding { active, permissions: None }))
	}

	fn stream_of(
		streams: &Arc<WatchStreams>,
		caller: UserId,
		standing: &Arc<Mutex<UserStanding>>,
		lifetime: Option<Duration>,
	) -> (BookWatch<Fake>, watch::Sender<u64>, Arc<Mutex<Option<u64>>>) {
		let (tx, rx) = watch::channel(0);
		let mut receiver = rx;
		receiver.mark_changed();
		let mut ticker = tokio::time::interval_at(Instant::now() + PERIOD, PERIOD);
		ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
		let threshold = Arc::new(Mutex::new(None));
		let state = WatchState {
			backend: Fake {
				standing: Arc::clone(standing),
				threshold: Arc::clone(&threshold),
			},
			receiver,
			ticker,
			deadline: lifetime.map(|left| Instant::now() + left),
			slot: Some(open_slot(streams, caller).unwrap()),
			finished: false,
		};
		(BookWatch::new(state), tx, threshold)
	}

	#[tokio::test(start_paused = true)]
	async fn a_holder_suspended_mid_stream_is_closed_on_the_next_recheck() {
		let streams = WatchStreams::new(4, PERIOD).unwrap();
		let caller = UserId::new();
		let holder = standing(true);
		let (mut stream, _feed, _threshold) = stream_of(&streams, caller, &holder, None);

		assert!(stream.next().await.unwrap().is_ok(), "the first frame comes at once");
		holder.lock().unwrap().active = false;
		// Nothing is sent between rechecks: the book is quiet and the timer has not fired.
		assert!(tokio::time::timeout(PERIOD / 2, stream.next()).await.is_err());

		let closing = stream.next().await.unwrap().unwrap_err();
		assert_eq!(closing.code(), Code::PermissionDenied);
		assert_eq!(streams.open_for(caller), 0, "the slot is free as soon as the stream closes");
		assert!(stream.next().await.is_none(), "nothing follows the closing status");
	}

	#[tokio::test(start_paused = true)]
	async fn an_active_holder_keeps_the_stream_across_rechecks() {
		let streams = WatchStreams::new(4, PERIOD).unwrap();
		let (mut stream, feed, _threshold) = stream_of(&streams, UserId::new(), &standing(true), None);
		assert!(stream.next().await.unwrap().is_ok());
		tokio::time::advance(PERIOD * 3).await;
		feed.send_replace(1);
		assert!(stream.next().await.unwrap().is_ok(), "rechecks passed silently and the next publish still frames");
	}

	#[tokio::test(start_paused = true)]
	async fn the_stream_ends_when_the_token_expires() {
		let streams = WatchStreams::new(4, PERIOD).unwrap();
		let caller = UserId::new();
		let (mut stream, _feed, _threshold) = stream_of(&streams, caller, &standing(true), Some(Duration::from_secs(30)));
		assert!(stream.next().await.unwrap().is_ok());
		let closing = stream.next().await.unwrap().unwrap_err();
		assert_eq!(closing.code(), Code::Unauthenticated);
		assert_eq!(streams.open_for(caller), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn dropping_a_stream_mid_wait_releases_its_slot() {
		let streams = WatchStreams::new(1, PERIOD).unwrap();
		let caller = UserId::new();
		let (mut stream, _feed, _threshold) = stream_of(&streams, caller, &standing(true), None);
		assert!(stream.next().await.unwrap().is_ok());
		// Parked on the next change, the way a cancelled RPC leaves it.
		assert!(tokio::time::timeout(Duration::from_secs(1), stream.next()).await.is_err());
		assert_eq!(streams.open_for(caller), 1);
		drop(stream);
		assert_eq!(streams.open_for(caller), 0);
		assert!(open_slot(&streams, caller).is_ok());
	}

	#[test]
	fn a_token_below_the_floor_is_revoked_and_no_floor_is_a_pass() {
		assert_eq!(not_revoked(2, Some(3)).unwrap_err().code(), Code::Unauthenticated);
		assert!(not_revoked(3, Some(3)).is_ok());
		assert!(not_revoked(0, None).is_ok());
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_revoked_mid_stream_closes_it_on_the_next_recheck() {
		let streams = WatchStreams::new(4, PERIOD).unwrap();
		let caller = UserId::new();
		let (mut stream, _feed, threshold) = stream_of(&streams, caller, &standing(true), None);
		assert!(stream.next().await.unwrap().is_ok());
		// "Revoke all" raises the floor above the claim the stream was opened with.
		*threshold.lock().unwrap() = Some(CLAIM + 1);
		let closing = stream.next().await.unwrap().unwrap_err();
		assert_eq!(closing.code(), Code::Unauthenticated);
		assert_eq!(streams.open_for(caller), 0);
		assert!(stream.next().await.is_none());
	}
}
