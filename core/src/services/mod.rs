//! invest-core's inbound **driving adapter** — its gRPC service surface (tonic). A closed,
//! internal surface: the invest BFF (a follow-up) is its caller, forwarding the user's
//! access token for the `invest` relying party.
//!
//! `TraceLayer` emits a span per request through the same `tracing` subscriber (and the
//! Sentry integration). No gRPC-Web and no HTTP/1.1: no browser ever reaches this port.

use std::{future::Future, net::SocketAddr};

use evconcierge_auth::{Verifier, grpc_auth_layer};
use invest_contracts::invest::v1::{allocations_service_server::AllocationsServiceServer, funds_service_server::FundsServiceServer};
use tonic::transport::Server;
use tower::Layer;
use tower_http::trace::{DefaultOnFailure, TraceLayer};

use crate::{
	AppState,
	services::{allocations::AllocationsSvc, funds::FundsSvc},
};

pub mod allocations;
pub mod funds;
pub(crate) mod support;

/// Build the tonic server and serve it on `addr` until `shutdown` fires.
///
/// `serve_with_shutdown` drains in-flight requests on the shutdown signal rather than
/// aborting them, so a deploy returns them cleanly instead of erroring the caller.
///
/// Authorization: each data service is wrapped in concierge's auth layer, which verifies
/// the bearer token locally against concierge's JWKS (`verifier`) and injects the verified
/// claims; the handlers then gate on the caller's `invest:*` permissions. The standard
/// `grpc.health.v1.Health` is left **unwrapped** so kubelet's gRPC probes stay public —
/// an infrastructure probe, not a data service.
pub async fn serve(addr: SocketAddr, state: AppState, verifier: Verifier, shutdown: impl Future<Output = ()>) -> Result<(), tonic::transport::Error> {
	let auth = grpc_auth_layer(verifier);
	let (health_reporter, std_health) = tonic_health::server::health_reporter();
	health_reporter.set_service_status("", tonic_health::ServingStatus::Serving).await;
	Server::builder()
		.layer(TraceLayer::new_for_grpc().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)))
		.add_service(std_health)
		.add_service(auth.layer(AllocationsServiceServer::new(AllocationsSvc::new(state.clone()))))
		.add_service(auth.layer(FundsServiceServer::new(FundsSvc::new(state))))
		.serve_with_shutdown(addr, shutdown)
		.await
}
