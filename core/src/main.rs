//! invest-core's composition root: read the config, dial Postgres, TigerBeetle and banking,
//! apply the migrations, and run the gRPC server, the outbox relay and the fee sweeper as
//! branches of one structured `join!` until a signal.

use std::{future::Future, sync::Arc};

use color_eyre::eyre::{Context, ensure, eyre};
use ev::error_monitoring::{self, Config as SentryConfig};
use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
use invest_core::{
	AppState,
	application::book::BookFeed,
	config,
	infrastructure::{
		allocations::PgAllocations,
		book::PgBook,
		db,
		fee_policy_changes::PgFeePolicyChanges,
		fee_sweeper::FeeSweeper,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		issuance::PgUnitIssuances,
		ledger::{self, TbLedger},
		money::{Unconfigured, grpc::ScopeLedgerClient},
		nav::PgNav,
		positions::PgFundPositions,
		redemptions::PgRedemptions,
		relay::Relay,
		subscriptions::PgSubscriptions,
		tigerbeetle::TigerBeetle,
		users::PgUsers,
	},
	ports::{Ledger, MoneyLedger, fees::FeePorts},
	services,
};
use invest_domain::book::PriceTimeEngine;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

// Sentry must be initialised before the async runtime starts — no #[tokio::main].
fn main() -> color_eyre::Result<()> {
	color_eyre::install()?;
	// Before any TLS client is built — see `crypto.rs`.
	invest_core::crypto::install_crypto_provider();
	dotenvy::dotenv().ok();

	let command = Command::from_args(std::env::args().skip(1));
	// The deploy contract, straight out of the image: the gitops preflight runs this
	// against the built image and diffs it with the cluster Secret's keys, so a missing
	// variable is caught before the rollout, not as a CrashLoopBackOff after it.
	if let Command::PrintRequiredVars(profile) = &command {
		for var in config::AppConfig::required_var_names(profile) {
			println!("{var}");
		}
		return Ok(());
	}

	// One env read at boot; every missing/invalid variable fails HERE, in one aggregate
	// error, before anything binds — exiting 78 (EX_CONFIG), so "the config is wrong" is
	// distinguishable from a dependency blip.
	let config = ev::settings::or_exit(config::AppConfig::from_env());

	// Guard must stay alive for the duration of main — dropping it flushes events.
	let _sentry_guard = error_monitoring::init(&SentryConfig {
		dsn: config.sentry_dsn.clone(),
		environment: config.app_env.clone(),
		release: error_monitoring::release_name!().map(|r| r.into_owned()),
		// Several services share one Sentry project; this is what tells them apart.
		service: Some("invest-core".to_string()),
		traces_sample_rate: SentryConfig::traces_sample_rate_for(&config.app_env),
	});
	// Held for the process lifetime — dropping flushes OTel logs/traces.
	let _otel_guard = init_tracing(&config.app_env);

	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("failed to build tokio runtime")?;
	runtime.block_on(run(config))
}

async fn run(config: config::AppConfig) -> color_eyre::Result<()> {
	// Asked before any infra dial, so a misconfigured release dies at once rather than
	// after a connect timeout.
	let scope_ledger = config.scope_ledger().context("invalid scope ledger configuration")?;
	if scope_ledger.is_none() {
		// Not refused in production: banking's listener and invest's client certificate
		// are not provisioned yet, and invest-core serves the registry and issuance without
		// them. Every cash read answers UNAVAILABLE until they are — loud, not wrong.
		tracing::warn!("no scope ledger configured (SCOPE_LEDGER_ADDR unset) — the reserved allocations' NAV is unavailable until it is");
	}
	let verifier_config = VerifierConfig {
		issuer: config.auth_issuer.clone(),
		audiences: vec![config.auth_audience.clone()],
		allowed_types: vec![TokenType::Access],
		jwks_grpc_endpoint: config.auth_jwks_grpc_endpoint.clone(),
	};
	// concierge's own `assert_plane` would refuse an `invest` audience (it insists on the
	// word "concierge"); what it guards — a verifier pointed at banking's issuer — is asked
	// here directly.
	ensure!(
		verifier_config.issuer.contains("concierge"),
		"AUTH_ISSUER {} is not the concierge plane's — invest-core accepts concierge's tokens only",
		verifier_config.issuer
	);
	let verifier = Verifier::try_new(verifier_config).map_err(|err| eyre!("invalid concierge JWKS endpoint: {err}"))?;

	let pool = db::connect_sized(&config.database_url, config.db_max_connections)
		.await
		.context("failed to connect to Postgres")?;
	db::migrate(&pool).await.context("failed to apply migrations")?;
	// The relay gets its own small pool so request traffic and unit dispatch can't exhaust
	// each other's connections.
	let relay_pool = db::connect_sized(&config.database_url, config.relay_db_max_connections)
		.await
		.context("failed to connect the relay's Postgres pool")?;

	let cluster_id: u128 = config.tigerbeetle_cluster_id.parse().context("TIGERBEETLE_CLUSTER_ID must be a u128")?;
	let tigerbeetle = Arc::new(TigerBeetle::connect(cluster_id, &config.tigerbeetle_address)?);
	let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
	ledger::probe(ledger.as_ref()).await.context("TigerBeetle did not answer")?;

	let money: Arc<dyn MoneyLedger> = match &scope_ledger {
		Some(scope_ledger) => Arc::new(ScopeLedgerClient::connect(scope_ledger)?),
		None => Arc::new(Unconfigured),
	};

	let relay_notify = Arc::new(Notify::new());
	let relay = Relay::new(relay_pool, ledger.clone(), money.clone(), relay_notify.clone());
	let fees = FeePorts {
		policies: Arc::new(PgFeePolicies::new(pool.clone())),
		changes: Arc::new(PgFeePolicyChanges::new(pool.clone())),
		accruals: Arc::new(PgPositionAccruals::new(pool.clone())),
		assessments: Arc::new(PgFeeAssessments::new(pool.clone())),
		settlements: Arc::new(PgFeeSettlements::new(pool.clone())),
	};
	let nav = Arc::new(PgNav::new(pool.clone()));
	let fee_sweeper = FeeSweeper::new(
		fees.policies.clone(),
		fees.changes.clone(),
		fees.accruals.clone(),
		fees.assessments.clone(),
		ledger.clone(),
		money.clone(),
		nav.clone(),
		relay_notify.clone(),
	);
	let state = AppState {
		pool: pool.clone(),
		ledger: ledger.clone(),
		money,
		users: Arc::new(PgUsers::new(pool.clone())),
		allocations: Arc::new(PgAllocations::new(pool.clone())),
		issuances: Arc::new(PgUnitIssuances::new(pool.clone())),
		subscriptions: Arc::new(PgSubscriptions::new(pool.clone())),
		redemptions: Arc::new(PgRedemptions::new(pool.clone(), ledger.clone())),
		book: Arc::new(PgBook::new(pool.clone())),
		book_engine: Arc::new(PriceTimeEngine),
		book_feed: BookFeed::new(),
		fees,
		nav,
		positions: Arc::new(PgFundPositions::new(pool)),
		relay_notify,
	};

	tracing::info!(addr = %config.grpc_addr, "invest-core serving");
	// Graceful shutdown, structured (no detached spawns): the server, the relay and the fee
	// sweeper run as branches of one `join!` on this task. A signal cancels a shared token;
	// the server drains in-flight calls, the workers stop at a batch boundary, and `join!`
	// waits for all of them.
	let shutdown = CancellationToken::new();
	let ((), served, relayed, swept) = tokio::join!(
		await_signal(shutdown.clone()),
		branch(&shutdown, "gRPC server", services::serve(config.grpc_addr, state, verifier, shutdown.clone().cancelled_owned())),
		branch(&shutdown, "relay", infallible(relay.run(shutdown.clone()))),
		branch(&shutdown, "fee sweeper", infallible(fee_sweeper.run(shutdown.clone()))),
	);
	served.and(relayed).and(swept)
}

/// Run one branch of the root `join!`: whatever ends it — an error or a clean return —
/// cancels the others, so the process never runs half its tasks.
async fn branch<E: std::fmt::Display>(shutdown: &CancellationToken, name: &str, fut: impl Future<Output = Result<(), E>>) -> color_eyre::Result<()> {
	let result = fut.await.map_err(|err| eyre!("{name} error: {err}"));
	if let Err(err) = &result {
		tracing::error!("{err}");
	}
	shutdown.cancel();
	result
}

/// Adapt an infallible loop task (`-> ()`) to the `Result` shape [`branch`] expects.
async fn infallible(fut: impl Future<Output = ()>) -> Result<(), std::convert::Infallible> {
	fut.await;
	Ok(())
}

/// What the binary was asked to do. Hand-rolled: one flag is not worth an argument-parser
/// dependency.
#[derive(Debug, PartialEq)]
enum Command {
	/// No arguments: serve.
	Serve,
	/// `--print-required-vars[=PROFILE]` (default `production`).
	PrintRequiredVars(String),
}

impl Command {
	fn from_args(mut args: impl Iterator<Item = String>) -> Self {
		const FLAG: &str = "--print-required-vars";
		let Some(arg) = args.next() else {
			return Self::Serve;
		};
		match arg.split_once('=') {
			Some((FLAG, profile)) => Self::PrintRequiredVars(profile.to_string()),
			Some(_) => Self::Serve,
			None if arg == FLAG => Self::PrintRequiredVars(args.next().unwrap_or_else(|| "production".to_string())),
			None => Self::Serve,
		}
	}
}

/// Resolve on the first of `SIGINT` (ctrl_c), `SIGTERM`, or a peer cancelling `shutdown`,
/// then cancel the token. A failed handler registration is logged and this branch idles on
/// the token (the process can still be force-killed) rather than tearing down at boot.
async fn await_signal(shutdown: CancellationToken) {
	use tokio::signal::unix::{SignalKind, signal};
	match signal(SignalKind::terminate()) {
		Ok(mut term) => {
			tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				result = tokio::signal::ctrl_c() => {
					if let Err(err) = result {
						tracing::error!("failed to listen for ctrl_c: {err}");
						shutdown.cancelled().await;
						return;
					}
				},
				_ = term.recv() => {},
			}
		}
		Err(err) => {
			tracing::error!("failed to install SIGTERM handler: {err}");
			shutdown.cancelled().await;
			return;
		}
	}
	tracing::info!("shutdown signal received — draining");
	shutdown.cancel();
}

// Returns the OTel guard (flushes/shuts down on drop); bind it in `main`. `None` when
// OTEL_EXPORTER_OTLP_ENDPOINT is unset — the layers are then inert.
fn init_tracing(environment: &str) -> Option<ev::otel::Telemetry> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,invest_core=debug"));
	let (otel_guard, otel_layers) = ev::otel::telemetry(&ev::otel::Config {
		environment: environment.to_string(),
		traces_sample_rate: ev::otel::Config::traces_sample_rate_for(environment),
	})
	.unzip();
	tracing_subscriber::registry()
		.with(filter)
		.with(fmt::layer().json())
		.with(error_monitoring::tracing_layer())
		.with(otel_layers)
		.init();
	otel_guard
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(args: &[&str]) -> Command {
		Command::from_args(args.iter().map(|a| (*a).to_string()))
	}

	#[test]
	fn the_command_line_is_serve_or_the_preflight_flag() {
		assert_eq!(parse(&[]), Command::Serve);
		assert_eq!(parse(&["--print-required-vars"]), Command::PrintRequiredVars("production".into()));
		assert_eq!(parse(&["--print-required-vars=dev"]), Command::PrintRequiredVars("dev".into()));
		assert_eq!(parse(&["--print-required-vars", "dev"]), Command::PrintRequiredVars("dev".into()));
	}
}
