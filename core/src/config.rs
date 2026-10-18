use std::{net::SocketAddr, sync::Arc, time::Duration};

use color_eyre::eyre::{Context, Result, ensure};
use invest_domain::book::TakerFeeRiseTerms;

use crate::{
	application::book::WatchStreams,
	infrastructure::{concierge::ConciergeStreamsConfig, money::grpc::ScopeLedgerConfig},
};

ev::settings! {
	/// invest-core's configuration, read from the environment only (`dotenvy` loads `.env`
	/// first in dev; prod values ride the container env — the image's baked topology
	/// contract plus gitops' k8s Secret `envFrom`). Every missing/invalid variable is
	/// reported in one aggregate error before anything binds, and an empty string counts as
	/// unset.
	pub struct AppConfig {
		database_url: String,
		/// The gRPC listener. Loopback by default: an internal seam reached by the invest
		/// BFF; production binds wider behind the cluster's network policy.
		grpc_addr: SocketAddr = "127.0.0.1:50071",
		/// `None` ⇒ error monitoring is a silent no-op (`ev::error_monitoring::init` returns
		/// no guard). Optional even in production: invest's own Sentry project is not created
		/// yet (EV-Fund/invest#18), so the DSN is added to the deploy env later and then just
		/// starts working.
		sentry_dsn: Option<String>,
		app_env: String,
		/// invest's own TigerBeetle cluster (e.g. `"127.0.0.1:3034"`), never banking's.
		tigerbeetle_address: String,
		/// TigerBeetle cluster id (a u128, so it rides as a string).
		tigerbeetle_cluster_id: String,
		/// Max connections for the request-serving Postgres pool.
		db_max_connections: u32 = "10",
		/// Max connections for the outbox relay's own pool, so request traffic and unit
		/// dispatch can't exhaust each other. The relay is a single worker: one drain
		/// connection plus the lock-holding one.
		relay_db_max_connections: u32 = "3",
		/// The issuer of the access tokens invest-core accepts — concierge's.
		auth_issuer: String = "https://auth.concierge.ev",
		/// The audience they carry: concierge's relying-party client for invest.
		auth_audience: String = "invest",
		/// concierge's gRPC endpoint, dialled for its JWKS.
		auth_jwks_grpc_endpoint: String,
		/// banking's `scope.v1.ScopeLedgerService` (`https://host:50054`). Unset ⇒ no money
		/// ledger: every read of cash (the reserved allocations' NAV) answers UNAVAILABLE
		/// rather than a guess. Set ⇒ every `SCOPE_LEDGER_*` below is required.
		scope_ledger_addr: Option<String>,
		/// The CA banking's scope-ledger certificate chains to (PEM file).
		scope_ledger_tls_ca_pem_file: Option<String>,
		/// invest's client certificate and key for the mTLS handshake (PEM files).
		scope_ledger_tls_cert_pem_file: Option<String>,
		scope_ledger_tls_key_pem_file: Option<String>,
		/// The name to verify banking's certificate against, when it is not the host of
		/// `SCOPE_LEDGER_ADDR`.
		scope_ledger_tls_domain: Option<String>,
		/// concierge's gRPC (`http://host:55670`, or its TLS listener `https://host:55672`),
		/// pulled for the lifecycle and scope streams the `users` projection is written
		/// from. Unset ⇒ the projection does not run and nobody is known to invest: every
		/// permission check and every mint fails closed. Set ⇒ `CONCIERGE_CLIENT_SECRET` is
		/// required.
		concierge_grpc_addr: Option<String>,
		/// The relying-party client invest reads the streams as; concierge filters them to
		/// what this client's scope may see.
		concierge_client_id: String = "invest",
		/// That client's secret — the value concierge holds as `RP_CLIENT_SECRET_INVEST`.
		#[secret]
		concierge_client_secret: Option<String>,
		/// The CA concierge's TLS listener chains to (PEM file). Required for an `https://`
		/// address: the listener's certificate is cluster-internal, no public root vouches for it.
		concierge_tls_ca_pem_file: Option<String>,
		/// The name to verify concierge's certificate against, when it is not the host of
		/// `CONCIERGE_GRPC_ADDR`.
		concierge_tls_domain: Option<String>,
		/// Where `AuthService.PublishCatalog` is reached, when that is not
		/// `CONCIERGE_GRPC_ADDR`. concierge serves it on its open listener only
		/// (`http://…:55670`); its TLS listener mounts the service seams alone, so a client
		/// on `https://…:55672` is answered `Unimplemented`. Unset ⇒ the catalog goes over
		/// `CONCIERGE_GRPC_ADDR`. Streams and the scope catalog never use it.
		concierge_catalog_grpc_addr: Option<String>,
		/// How long a taker-fee RISE on a book waits before it binds (seconds, at least 1);
		/// holders are notified in that time. A cut binds at once. Defaults to one day, the
		/// notice a fund's fee-policy change gives.
		book_taker_fee_rise_delay_secs: i64 = "86400",
		/// The most a single taker-fee rise may add to the fee in force (bps, 1..=100, the
		/// hard cap `MAX_TAKER_FEE_BPS`). Larger moves are made in steps, each with its own notice.
		book_taker_fee_rise_ceiling_bps: u32 = "25",
		/// How many `WatchBook` streams one caller may hold open; past it the stream is
		/// refused `RESOURCE_EXHAUSTED`. At least 1 (the BFF's own cap is 4 per session).
		watchbook_max_streams_per_caller: u32 = "4",
		/// How often a live `WatchBook` stream re-checks that its holder is still active
		/// and may still see the book. At least 1.
		watchbook_recheck_secs: u64 = "60",
		/// How old a `book_order:` hold in banking must be, with no order row behind it,
		/// before the hold reaper voids it. At least 60: a buy's hold and its order row are
		/// written moments apart, and the floor keeps a placement in flight from being taken.
		hold_reaper_after_secs: u64 = "900",
	}
}

impl AppConfig {
	/// The pacing of taker-fee rises, or why the two variables do not describe one — the
	/// same "the config is wrong" as the rest of the wiring, so it exits 78 too.
	pub fn book_taker_fee_rise(&self) -> Result<TakerFeeRiseTerms> {
		TakerFeeRiseTerms::new(self.book_taker_fee_rise_delay_secs, self.book_taker_fee_rise_ceiling_bps)
			.map_err(|err| color_eyre::eyre::eyre!("invalid BOOK_TAKER_FEE_RISE_DELAY_SECS / BOOK_TAKER_FEE_RISE_CEILING_BPS: {err}"))
	}

	/// The `WatchBook` limits. A zero is refused here rather than clamped: a cap of 0 would
	/// silently disable the stream and a recheck of 0 seconds would spin the timer.
	pub fn watch_streams(&self) -> Result<Arc<WatchStreams>> {
		WatchStreams::new(self.watchbook_max_streams_per_caller, Duration::from_secs(self.watchbook_recheck_secs)).map_err(|err| color_eyre::eyre::eyre!("{err}"))
	}

	/// The hold reaper's age floor. Refused under a minute rather than clamped: a smaller
	/// value would race a placement still writing its order row.
	pub fn hold_reaper_after(&self) -> Result<Duration> {
		ensure!(
			self.hold_reaper_after_secs >= 60,
			"HOLD_REAPER_AFTER_SECS must be at least 60, got {}",
			self.hold_reaper_after_secs
		);
		Ok(Duration::from_secs(self.hold_reaper_after_secs))
	}

	/// The concierge stream client's configuration, or `None` when none is configured.
	/// Half a configuration is refused, as for the scope ledger: an address without its
	/// secret is refused by concierge on every pull, and a secret without an address is a
	/// deploy that forgot one line.
	pub fn concierge_streams(&self) -> Result<Option<ConciergeStreamsConfig>> {
		let Some(addr) = &self.concierge_grpc_addr else {
			ensure!(
				self.concierge_client_secret.is_none() && self.concierge_tls_ca_pem_file.is_none() && self.concierge_tls_domain.is_none() && self.concierge_catalog_grpc_addr.is_none(),
				"CONCIERGE_GRPC_ADDR is unset but its credentials or CONCIERGE_CATALOG_GRPC_ADDR are set — set the address or drop them"
			);
			return Ok(None);
		};
		let secure = addr.starts_with("https://");
		ensure!(secure || addr.starts_with("http://"), "CONCIERGE_GRPC_ADDR {addr} must be http:// or https://");
		let client_secret = self
			.concierge_client_secret
			.clone()
			.ok_or_else(|| color_eyre::eyre::eyre!("CONCIERGE_GRPC_ADDR is set, so CONCIERGE_CLIENT_SECRET is required"))?;
		let ca_pem = match (&self.concierge_tls_ca_pem_file, secure) {
			(Some(path), true) => Some(std::fs::read_to_string(path).with_context(|| format!("failed to read CONCIERGE_TLS_CA_PEM_FILE at {path}"))?),
			(None, true) => color_eyre::eyre::bail!("CONCIERGE_GRPC_ADDR {addr} is https://, so CONCIERGE_TLS_CA_PEM_FILE is required"),
			(Some(_), false) => color_eyre::eyre::bail!("CONCIERGE_TLS_CA_PEM_FILE is set but CONCIERGE_GRPC_ADDR {addr} is not https://"),
			(None, false) => None,
		};
		let catalog_addr = catalog_addr_from(self.concierge_catalog_grpc_addr.as_deref(), ca_pem.is_some())?;
		Ok(Some(ConciergeStreamsConfig {
			addr: addr.clone(),
			catalog_addr,
			client_id: self.concierge_client_id.clone(),
			client_secret,
			ca_pem,
			tls_domain: self.concierge_tls_domain.clone(),
		}))
	}

	/// The scope-ledger client's configuration, or `None` when none is configured. Half a
	/// configuration is refused: an address without its credentials would fail every call
	/// at runtime, and credentials without an address are a deploy that forgot one line.
	pub fn scope_ledger(&self) -> Result<Option<ScopeLedgerConfig>> {
		scope_ledger_from(
			self.scope_ledger_addr.as_deref(),
			[
				("SCOPE_LEDGER_TLS_CA_PEM_FILE", self.scope_ledger_tls_ca_pem_file.as_deref()),
				("SCOPE_LEDGER_TLS_CERT_PEM_FILE", self.scope_ledger_tls_cert_pem_file.as_deref()),
				("SCOPE_LEDGER_TLS_KEY_PEM_FILE", self.scope_ledger_tls_key_pem_file.as_deref()),
			],
			self.scope_ledger_tls_domain.as_deref(),
		)
	}
}

/// The rules of `CONCIERGE_CATALOG_GRPC_ADDR` on plain values: `http://` or `https://`, and
/// an `https://` one is verified against the same CA as the main address, so that CA must
/// be configured.
fn catalog_addr_from(addr: Option<&str>, has_ca: bool) -> Result<Option<String>> {
	let Some(addr) = addr else { return Ok(None) };
	let secure = addr.starts_with("https://");
	ensure!(secure || addr.starts_with("http://"), "CONCIERGE_CATALOG_GRPC_ADDR {addr} must be http:// or https://");
	ensure!(!secure || has_ca, "CONCIERGE_CATALOG_GRPC_ADDR {addr} is https://, so CONCIERGE_TLS_CA_PEM_FILE is required");
	Ok(Some(addr.to_owned()))
}

/// The rules of [`AppConfig::scope_ledger`] on plain values, so they are testable without
/// touching the process environment. `files` is the CA, the client certificate and its key.
fn scope_ledger_from(addr: Option<&str>, files: [(&'static str, Option<&str>); 3], tls_domain: Option<&str>) -> Result<Option<ScopeLedgerConfig>> {
	let Some(addr) = addr else {
		let stray: Vec<&str> = files.iter().filter(|(_, v)| v.is_some()).map(|(name, _)| *name).collect();
		ensure!(
			stray.is_empty(),
			"SCOPE_LEDGER_ADDR is unset but its TLS files are set ({stray:?}) — set the address or drop them"
		);
		return Ok(None);
	};
	ensure!(
		addr.starts_with("https://"),
		"SCOPE_LEDGER_ADDR {addr} must be https:// — banking's scope ledger speaks only mTLS"
	);
	let read = |(name, path): (&str, Option<&str>)| -> Result<String> {
		let path = path.ok_or_else(|| color_eyre::eyre::eyre!("SCOPE_LEDGER_ADDR is set, so {name} is required"))?;
		std::fs::read_to_string(path).with_context(|| format!("failed to read {name} at {path}"))
	};
	let [ca, cert, key] = files;
	Ok(Some(ScopeLedgerConfig {
		addr: addr.to_owned(),
		ca_pem: read(ca)?,
		client_cert_pem: read(cert)?,
		client_key_pem: read(key)?,
		tls_domain: tls_domain.map(str::to_owned),
	}))
}

#[cfg(test)]
mod scope_ledger_tests {
	use super::*;

	const NAMES: [&str; 3] = ["SCOPE_LEDGER_TLS_CA_PEM_FILE", "SCOPE_LEDGER_TLS_CERT_PEM_FILE", "SCOPE_LEDGER_TLS_KEY_PEM_FILE"];

	fn files(paths: [Option<&str>; 3]) -> [(&'static str, Option<&str>); 3] {
		[(NAMES[0], paths[0]), (NAMES[1], paths[1]), (NAMES[2], paths[2])]
	}

	fn pem(name: &str) -> String {
		let path = std::env::temp_dir().join(format!("invest-scope-ledger-config-{}-{name}.pem", std::process::id()));
		std::fs::write(&path, format!("pem of {name}")).unwrap();
		path.to_str().unwrap().to_owned()
	}

	#[test]
	fn nothing_set_is_no_ledger() {
		assert!(scope_ledger_from(None, files([None; 3]), None).unwrap().is_none());
	}

	#[test]
	fn tls_files_without_an_address_are_refused() {
		let err = scope_ledger_from(None, files([Some("/ca.pem"), None, None]), None).err().unwrap().to_string();
		assert!(err.contains("SCOPE_LEDGER_ADDR is unset") && err.contains("SCOPE_LEDGER_TLS_CA_PEM_FILE"), "{err}");
	}

	#[test]
	fn an_address_without_its_files_is_refused() {
		let err = scope_ledger_from(Some("https://banking:50054"), files([None; 3]), None).err().unwrap().to_string();
		assert!(err.contains("SCOPE_LEDGER_TLS_CA_PEM_FILE is required"), "{err}");
		let ca = pem("ca-only");
		let err = scope_ledger_from(Some("https://banking:50054"), files([Some(&ca), None, None]), None).err().unwrap().to_string();
		assert!(err.contains("SCOPE_LEDGER_TLS_CERT_PEM_FILE is required"), "{err}");
	}

	#[test]
	fn a_plaintext_address_is_refused() {
		let err = scope_ledger_from(Some("http://banking:50054"), files([None; 3]), None).err().unwrap().to_string();
		assert!(err.contains("must be https://"), "{err}");
	}

	#[test]
	fn an_address_with_its_three_files_is_a_config() {
		let (ca, cert, key) = (pem("ca"), pem("cert"), pem("key"));
		let config = scope_ledger_from(Some("https://banking:50054"), files([Some(&ca), Some(&cert), Some(&key)]), Some("banking.internal"))
			.unwrap()
			.unwrap();
		assert_eq!((config.ca_pem.as_str(), config.client_key_pem.as_str()), ("pem of ca", "pem of key"));
		assert_eq!(config.tls_domain.as_deref(), Some("banking.internal"));
	}
}

#[cfg(test)]
mod catalog_addr_tests {
	use super::*;

	#[test]
	fn unset_keeps_the_main_address() {
		assert_eq!(catalog_addr_from(None, false).unwrap(), None);
		assert_eq!(catalog_addr_from(None, true).unwrap(), None);
	}

	#[test]
	fn a_plaintext_address_needs_no_ca() {
		let addr = "http://concierge:55670";
		assert_eq!(catalog_addr_from(Some(addr), false).unwrap().as_deref(), Some(addr));
	}

	#[test]
	fn a_tls_address_needs_the_ca() {
		let err = catalog_addr_from(Some("https://concierge:55672"), false).err().unwrap().to_string();
		assert!(err.contains("CONCIERGE_TLS_CA_PEM_FILE is required"), "{err}");
		assert!(catalog_addr_from(Some("https://concierge:55672"), true).unwrap().is_some());
	}

	#[test]
	fn another_scheme_is_refused() {
		let err = catalog_addr_from(Some("concierge:55670"), true).err().unwrap().to_string();
		assert!(err.contains("must be http:// or https://"), "{err}");
	}
}
