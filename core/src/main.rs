//! invest-core's composition root: read the config, dial Postgres, TigerBeetle, banking and
//! concierge, apply the migrations, and run the gRPC server, the outbox relay, the fee
//! sweeper, the users projection, the money-event consumer, the hold reaper, the fee
//! approvals round and the catalog publication as branches of one structured `join!` until
//! a signal.

use std::{future::Future, process::ExitCode, sync::Arc, time::Duration};

use color_eyre::eyre::{Context, ensure, eyre};
use ev::error_monitoring::{self, Config as SentryConfig};
use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
use evconcierge_projection::ProjectionConfig;
use invest_core::{
	AppState,
	application::book::{BookFeed, WatchStreams},
	config,
	infrastructure::{
		allocations::PgAllocations,
		book::PgBook,
		concierge::{
			self, ConciergeProjection, ConciergeStreamsConfig,
			approvals::{ConciergeApprovals, UnconfiguredApprovals},
			catalog::{ClientCatalogPublisher, ClientScopeCatalogPublisher, publish_at_boot, publish_scope_at_boot},
		},
		db,
		fee_approvals::FeeApprovalsExecutor,
		fee_policy_changes::PgFeePolicyChanges,
		fee_sweeper::FeeSweeper,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		hold_reaper::HoldReaper,
		issuance::PgUnitIssuances,
		ledger::{self, TbLedger},
		money::{
			Unconfigured,
			grpc::{ScopeLedgerClient, ScopeLedgerConfig},
		},
		money_events::MoneyEventsConsumer,
		nav::PgNav,
		positions::PgFundPositions,
		redemptions::PgRedemptions,
		relay::Relay,
		subscriptions::PgSubscriptions,
		tigerbeetle::TigerBeetle,
		users::PgUsers,
	},
	ports::{
		Ledger, MoneyLedger, UserDirectory,
		approvals::Approvals,
		book::{BookStore, OrderPresence},
		catalog::{CATALOG_VERSION, CatalogPublisher, SCOPE_CATALOG_VERSION, ScopeCatalog, ScopeCatalogPublisher},
		fees::FeePorts,
	},
	services,
};
use invest_domain::{
	authz::PermissionCatalog,
	book::{PriceTimeEngine, TakerFeeRiseTerms},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The identity plane's issuer — the only one whose tokens invest-core accepts.
const CONCIERGE_ISSUER: &str = "https://auth.concierge.ev";
/// invest's relying-party client in concierge — the only audience accepted.
const INVEST_AUDIENCE: &str = "invest";

mod import_cmd;
mod window_import_cmd;

// Sentry must be initialised before the async runtime starts — no #[tokio::main].
fn main() -> color_eyre::Result<ExitCode> {
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
		return Ok(ExitCode::SUCCESS);
	}
	match command {
		Command::Import(args) => return import_cmd::run(args),
		Command::WindowImport(args) => return window_import_cmd::run(args),
		Command::Invalid(message) => {
			eprintln!("{message}");
			return Ok(ExitCode::from(2));
		}
		Command::Serve | Command::PrintRequiredVars(_) => {}
	}

	// One env read at boot; every missing/invalid variable fails HERE, in one aggregate
	// error, before anything binds — exiting 78 (EX_CONFIG), so "the config is wrong" is
	// distinguishable from a dependency blip.
	let config = ev::settings::or_exit(config::AppConfig::from_env());
	// The rules between variables `settings!` cannot state — an address needs its
	// credentials, a PEM file must be readable — are the same "the config is wrong", so they
	// exit 78 as well, before anything is dialled.
	let wiring = config.scope_ledger().context("invalid scope ledger configuration").and_then(|scope_ledger| {
		Ok((
			scope_ledger,
			config.concierge_streams().context("invalid concierge stream configuration")?,
			config.watch_streams().context("invalid WatchBook limits")?,
			config.book_taker_fee_rise()?,
			config.hold_reaper_after().context("invalid hold reaper configuration")?,
		))
	});
	let (scope_ledger, concierge_streams, watch_streams, book_taker_fee_rise, hold_reaper_after) = wiring.unwrap_or_else(|err| {
		eprintln!("{err:?}");
		std::process::exit(ev::settings::EX_CONFIG);
	});

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
	if std::env::var_os("SCOPE_LEDGER_SERVICE_TOKEN").is_some() {
		// A deploy that still ships the old secret must hear that it is ignored: invest-core
		// mints its token from banking now.
		tracing::warn!("SCOPE_LEDGER_SERVICE_TOKEN is set but ignored — remove it from the deploy, and revoke the token that was issued by hand");
	}
	if config.sentry_dsn.is_none() && config.app_env == "production" {
		tracing::warn!("no SENTRY_DSN: errors are not reported");
	}

	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("failed to build tokio runtime")?;
	runtime.block_on(run(config, scope_ledger, concierge_streams, watch_streams, book_taker_fee_rise, hold_reaper_after))?;
	Ok(ExitCode::SUCCESS)
}

async fn run(
	config: config::AppConfig,
	scope_ledger: Option<ScopeLedgerConfig>,
	concierge_streams: Option<ConciergeStreamsConfig>,
	watch_streams: Arc<WatchStreams>,
	book_taker_fee_rise: TakerFeeRiseTerms,
	hold_reaper_after: Duration,
) -> color_eyre::Result<()> {
	if scope_ledger.is_none() {
		// Not refused in production: banking's listener and invest's client certificate
		// are not provisioned yet, and invest-core serves the registry and issuance without
		// them. Every cash read answers UNAVAILABLE until they are — loud, not wrong.
		tracing::warn!("no scope ledger configured (SCOPE_LEDGER_ADDR unset) — the reserved allocations' NAV is unavailable until it is");
	}
	// Not refused: invest-core serves the registry without anyone known to it, and every
	// check that needs a person fails closed — loud, not wrong. A cleartext address in
	// production is refused by `concierge_streams` itself.
	if concierge_streams.is_none() {
		tracing::warn!(
			"no concierge streams configured (CONCIERGE_GRPC_ADDR unset) — the users projection does not run and the permission catalog is not published: nobody is known to invest, so every permission check and every mint is refused"
		);
	}
	let verifier_config = VerifierConfig {
		issuer: config.auth_issuer.clone(),
		audiences: vec![config.auth_audience.clone()],
		allowed_types: vec![TokenType::Access],
		jwks_grpc_endpoint: config.auth_jwks_grpc_endpoint.clone(),
	};
	// concierge's own `assert_plane` would refuse an `invest` audience (it insists on the
	// word "concierge"); what it guards — a verifier pointed at another plane's issuer, or
	// accepting tokens minted for another relying party — is asked here directly, and
	// exactly: a substring would admit any issuer that merely mentions concierge.
	ensure!(
		verifier_config.issuer == CONCIERGE_ISSUER,
		"AUTH_ISSUER {} is not the concierge plane's ({CONCIERGE_ISSUER}) — invest-core accepts concierge's tokens only",
		verifier_config.issuer
	);
	ensure!(
		verifier_config.audiences == [INVEST_AUDIENCE],
		"AUTH_AUDIENCE {:?} is not invest's ({INVEST_AUDIENCE}) — invest-core accepts tokens minted for invest only",
		verifier_config.audiences
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

	let scope_ledger_client = scope_ledger.as_ref().map(ScopeLedgerClient::connect).transpose()?.map(Arc::new);
	let money: Arc<dyn MoneyLedger> = match &scope_ledger_client {
		Some(client) => client.clone(),
		None => Arc::new(Unconfigured),
	};

	let projection = match &concierge_streams {
		Some(streams) => {
			// Its own small pool, as the relay's: the lock-holding connection plus the engine's
			// sequential reads and writes, apart from request traffic.
			let pool = db::connect_sized(&config.database_url, 3)
				.await
				.context("failed to connect the users projection's Postgres pool")?;
			Some(ConciergeProjection::new(pool, concierge::connect(streams)?, ProjectionConfig::default()))
		}
		None => None,
	};
	let catalog_publisher = concierge_streams.as_ref().map(ClientCatalogPublisher::connect).transpose()?;
	let scope_catalog_publisher = concierge_streams.as_ref().map(ClientScopeCatalogPublisher::connect).transpose()?;
	let approvals: Arc<dyn Approvals> = match &concierge_streams {
		Some(streams) => Arc::new(ConciergeApprovals::connect(streams)?),
		None => Arc::new(UnconfiguredApprovals),
	};

	let relay_notify = Arc::new(Notify::new());
	let relay = Relay::new(relay_pool, ledger.clone(), money.clone(), relay_notify.clone());
	let book_feed = BookFeed::new();
	// Without a scope ledger there is no hold to list: the branch idles, as the consumer's.
	let hold_reaper = scope_ledger_client
		.is_some()
		.then(|| HoldReaper::new(money.clone(), Arc::new(PgBook::new(pool.clone())) as Arc<dyn OrderPresence>, hold_reaper_after));
	let money_events = match scope_ledger_client {
		Some(client) => {
			// Its own small pool, as the projection's: the lock-holding connection plus one
			// transaction per event, apart from request traffic.
			let pool = db::connect_sized(&config.database_url, 3)
				.await
				.context("failed to connect the money-event consumer's Postgres pool")?;
			Some(MoneyEventsConsumer::new(pool, client, money.clone(), relay_notify.clone(), book_feed.clone()))
		}
		None => None,
	};
	let fees = FeePorts {
		policies: Arc::new(PgFeePolicies::new(pool.clone())),
		changes: Arc::new(PgFeePolicyChanges::new(pool.clone())),
		approvals: approvals.clone(),
		accruals: Arc::new(PgPositionAccruals::new(pool.clone())),
		assessments: Arc::new(PgFeeAssessments::new(pool.clone())),
		settlements: Arc::new(PgFeeSettlements::new(pool.clone(), ledger.clone())),
	};
	let nav = Arc::new(PgNav::new(pool.clone()));
	let allocations = Arc::new(PgAllocations::new(pool.clone()));
	let book: Arc<dyn BookStore> = Arc::new(PgBook::new(pool.clone()));
	let fee_sweeper = FeeSweeper::new(
		allocations.clone(),
		fees.policies.clone(),
		fees.changes.clone(),
		fees.accruals.clone(),
		fees.assessments.clone(),
		ledger.clone(),
		money.clone(),
		nav.clone(),
		relay_notify.clone(),
	)
	.with_book(book.clone());
	let users: Arc<dyn UserDirectory> = Arc::new(PgUsers::new(pool.clone()));
	// Without concierge nothing is proposed, so there is nothing to follow: the branch idles.
	let fee_approvals = concierge_streams
		.is_some()
		.then(|| FeeApprovalsExecutor::new(approvals, fees.changes.clone(), fees.policies.clone(), users.clone()));
	let state = AppState {
		pool: pool.clone(),
		ledger: ledger.clone(),
		money,
		users,
		allocations,
		issuances: Arc::new(PgUnitIssuances::new(pool.clone())),
		subscriptions: Arc::new(PgSubscriptions::new(pool.clone())),
		redemptions: Arc::new(PgRedemptions::new(pool.clone(), ledger.clone())),
		book,
		book_taker_fee_rise,
		book_engine: Arc::new(PriceTimeEngine),
		book_feed,
		watch_streams,
		fees,
		nav,
		positions: Arc::new(PgFundPositions::new(pool)),
		relay_notify,
	};

	tracing::info!(addr = %config.grpc_addr, "invest-core serving");
	// Graceful shutdown, structured (no detached spawns): the server, the relay, the fee
	// sweeper, the users projection, the money-event consumer, the fee approvals round and
	// the catalog publication run as branches of one `join!` on this task. A signal cancels a shared token;
	// the server drains in-flight calls, the workers stop at a batch boundary, and `join!`
	// waits for all of them.
	let shutdown = CancellationToken::new();
	let ((), served, relayed, swept, projected, consumed, reaped, approved, published) = tokio::join!(
		await_signal(shutdown.clone()),
		branch(&shutdown, "gRPC server", services::serve(config.grpc_addr, state, verifier, shutdown.clone().cancelled_owned())),
		branch(&shutdown, "relay", infallible(relay.run(shutdown.clone()))),
		branch(&shutdown, "fee sweeper", infallible(fee_sweeper.run(shutdown.clone()))),
		branch(
			&shutdown,
			"users projection",
			infallible(async {
				match projection {
					Some(projection) => projection.run(shutdown.clone()).await,
					// Unconfigured: idle until shutdown, so the branch ending does not stop the rest.
					None => shutdown.cancelled().await,
				}
			})
		),
		branch(
			&shutdown,
			"money-event consumer",
			infallible(async {
				match money_events {
					Some(consumer) => consumer.run(shutdown.clone()).await,
					// No scope ledger: no events to pull; idle so the branch ending does not stop
					// the rest.
					None => shutdown.cancelled().await,
				}
			})
		),
		branch(
			&shutdown,
			"hold reaper",
			infallible(async {
				match hold_reaper {
					Some(reaper) => reaper.run(shutdown.clone()).await,
					None => shutdown.cancelled().await,
				}
			})
		),
		branch(
			&shutdown,
			"fee approvals",
			infallible(async {
				match fee_approvals {
					Some(executor) => executor.run(shutdown.clone()).await,
					None => shutdown.cancelled().await,
				}
			})
		),
		branch(
			&shutdown,
			"catalog publisher",
			infallible(async {
				// Side by side, so one catalog stuck on retries does not hold the other back.
				// Done, refused or cancelled — each already logged; nothing else waits on them.
				let permissions = async {
					if let Some(publisher) = &catalog_publisher {
						let _ = publish_at_boot(publisher as &dyn CatalogPublisher, &PermissionCatalog::invest(), CATALOG_VERSION, &shutdown).await;
					}
				};
				let scopes = async {
					if let Some(publisher) = &scope_catalog_publisher {
						let _ = publish_scope_at_boot(publisher as &dyn ScopeCatalogPublisher, &ScopeCatalog::invest(), SCOPE_CATALOG_VERSION, &shutdown).await;
					}
				};
				tokio::join!(permissions, scopes);
				// A one-shot: idle afterwards, so its end does not stop the rest.
				shutdown.cancelled().await;
			})
		),
	);
	served.and(relayed).and(swept).and(projected).and(consumed).and(reaped).and(approved).and(published)
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
	/// `import --file <path> [--dry-run] [--anchor …]` — the one-shot data-wave importer.
	Import(import_cmd::ImportArgs),
	/// `window-import --file <path> [--dry-run | --apply]` — banking's window export into invest.
	WindowImport(window_import_cmd::WindowImportArgs),
	/// `import` or `window-import` with arguments that do not parse; exits 2 instead of serving by accident.
	Invalid(String),
}

impl Command {
	fn from_args(mut args: impl Iterator<Item = String>) -> Self {
		const FLAG: &str = "--print-required-vars";
		let Some(arg) = args.next() else {
			return Self::Serve;
		};
		if arg == "import" {
			return import_cmd::ImportArgs::parse(args).map_or_else(Self::Invalid, Self::Import);
		}
		if arg == "window-import" {
			return window_import_cmd::WindowImportArgs::parse(args).map_or_else(Self::Invalid, Self::WindowImport);
		}
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

	/// The gitops preflight diffs the cluster Secret against `--print-required-vars`; listing
	/// `SENTRY_DSN` would block deploys until invest's Sentry project exists.
	#[test]
	fn error_monitoring_is_optional_in_production() {
		assert!(!config::AppConfig::required_var_names("production").contains(&"SENTRY_DSN".to_string()));
	}
}
