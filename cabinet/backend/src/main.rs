//! The invest cabinet BFF's composition root: read the config, dial Postgres (migrating
//! the `cabinet` schema), concierge and invest-core, and serve HTTP until a signal.

use std::sync::Arc;

use color_eyre::eyre::Context;
use ev::error_monitoring::{self, Config as SentryConfig};
use evconcierge_client::{ClientConfig, ConciergeClient};
use invest_cabinet_backend::{
	config::AppConfig,
	cookies::CookieNames,
	crypto, db, routes,
	session::Sessions,
	state::{AppState, Grpc},
};

// Sentry must be initialised before the async runtime starts — no `#[tokio::main]`.
fn main() -> color_eyre::Result<()> {
	color_eyre::install()?;
	// Before any TLS handshake — see `crypto.rs`.
	crypto::install_crypto_provider();
	dotenvy::dotenv().ok();

	// The deploy contract, straight out of the image: the gitops preflight runs this
	// against the built image and diffs it with the cluster Secret's keys, so a missing
	// variable is caught before the rollout, not as a CrashLoopBackOff after it.
	if let Some(profile) = print_required_vars_for() {
		for var in AppConfig::required_var_names(&profile) {
			println!("{var}");
		}
		return Ok(());
	}

	// One env read at boot; every missing or invalid variable fails HERE, in one aggregate
	// error, before anything binds — exiting 78 (EX_CONFIG), so "the config is wrong" is
	// distinguishable from a dependency blip.
	let config = ev::settings::or_exit(AppConfig::from_env());

	// Dropping the guard flushes events, so it lives as long as `main`.
	let _sentry_guard = error_monitoring::init(&SentryConfig {
		dsn: config.sentry_dsn.clone(),
		environment: config.app_env.clone(),
		release: error_monitoring::release_name!().map(|r| r.into_owned()),
		// Several services share one Sentry project; this is what tells them apart. Same
		// name as the binary and OTEL_SERVICE_NAME, so an issue, a trace and a log agree.
		service: Some("invest-cabinet-backend".to_string()),
		traces_sample_rate: SentryConfig::traces_sample_rate_for(&config.app_env),
	});
	let _otel_guard = init_tracing(&config.app_env);

	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.context("failed to build tokio runtime")?
		.block_on(run(config))
}

async fn run(config: AppConfig) -> color_eyre::Result<()> {
	let pool = db::connect(&config.database_url, config.db_max_connections).await?;
	db::migrate(&pool).await?;

	// Lazy: the BFF boots and serves its probe while concierge or invest-core are down.
	let concierge = ConciergeClient::connect_lazy(ClientConfig::new(
		config.concierge_grpc_addr.clone(),
		config.concierge_client_id.clone(),
		config.concierge_client_secret.clone(),
	))
	.context("invalid CONCIERGE_GRPC_ADDR")?;
	let grpc = Grpc::connect_lazy(&config.invest_core_grpc_addr).context("invalid INVEST_CORE_GRPC_ADDR")?;

	let bind = config.bind;
	tracing::info!(
		%bind,
		invest_core = %config.invest_core_grpc_addr,
		concierge = %config.concierge_grpc_addr,
		origin = %config.cabinet_origin(),
		"invest cabinet BFF listening"
	);
	let state = AppState {
		cookies: Arc::new(CookieNames::new(config.cookie_secure())),
		sessions: Arc::new(Sessions::new(pool, Arc::new(concierge))),
		grpc,
		config: Arc::new(config),
	};

	let listener = tokio::net::TcpListener::bind(bind).await.context("failed to bind HTTP listener")?;
	// Graceful on SIGTERM, which the kubelet sends on every rollout: the listener closes,
	// in-flight requests finish (each bounded by the router's deadline), then the process
	// exits; `terminationGracePeriodSeconds` is the SIGKILL backstop.
	axum::serve(listener, routes::router(state))
		.with_graceful_shutdown(shutdown_signal())
		.await
		.context("cabinet BFF HTTP server error")
}

/// Resolve on the first of `SIGINT` or `SIGTERM`. A failed SIGTERM registration is logged
/// and the wait falls back to ctrl_c alone rather than refusing to boot.
async fn shutdown_signal() {
	use tokio::signal::unix::{SignalKind, signal};
	match signal(SignalKind::terminate()) {
		Ok(mut term) => {
			tokio::select! {
				result = tokio::signal::ctrl_c() => {
					if let Err(err) = result {
						tracing::error!("failed to listen for ctrl_c: {err}");
						term.recv().await;
					}
				},
				_ = term.recv() => {},
			}
		}
		Err(err) => {
			tracing::error!("failed to install SIGTERM handler: {err}");
			if let Err(err) = tokio::signal::ctrl_c().await {
				tracing::error!("failed to listen for ctrl_c: {err}");
				std::future::pending::<()>().await;
			}
		}
	}
	tracing::info!("shutdown signal received — draining");
}

/// `--print-required-vars[=PROFILE]` (default `production`). Hand-rolled: the binary has no
/// other CLI surface.
fn print_required_vars_for() -> Option<String> {
	const FLAG: &str = "--print-required-vars";

	let mut args = std::env::args().skip(1);
	let arg = args.next()?;
	match arg.split_once('=') {
		Some((FLAG, profile)) => Some(profile.to_string()),
		Some(_) => None,
		None if arg == FLAG => Some(args.next().unwrap_or_else(|| "production".to_string())),
		None => None,
	}
}

/// Returns the OTel guard (flushes on drop); `None` when OTEL_EXPORTER_OTLP_ENDPOINT is
/// unset — the layers are then inert.
fn init_tracing(environment: &str) -> Option<ev::otel::Telemetry> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,invest_cabinet_backend=debug"));
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
