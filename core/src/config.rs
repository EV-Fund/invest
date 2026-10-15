use std::net::SocketAddr;

use color_eyre::eyre::{Context, Result, ensure};

use crate::infrastructure::{concierge::ConciergeStreamsConfig, money::grpc::ScopeLedgerConfig};

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
		/// The `typ=service` token for the client `invest` (until banking's
		/// `MintServiceToken` lands, provisioned out of band).
		#[secret]
		scope_ledger_service_token: Option<String>,
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
	}
}

impl AppConfig {
	/// The concierge stream client's configuration, or `None` when none is configured.
	/// Half a configuration is refused, as for the scope ledger: an address without its
	/// secret is refused by concierge on every pull, and a secret without an address is a
	/// deploy that forgot one line.
	pub fn concierge_streams(&self) -> Result<Option<ConciergeStreamsConfig>> {
		let Some(addr) = &self.concierge_grpc_addr else {
			ensure!(
				self.concierge_client_secret.is_none() && self.concierge_tls_ca_pem_file.is_none() && self.concierge_tls_domain.is_none(),
				"CONCIERGE_GRPC_ADDR is unset but its credentials are set — set the address or drop them"
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
		Ok(Some(ConciergeStreamsConfig {
			addr: addr.clone(),
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
		let files = [
			("SCOPE_LEDGER_TLS_CA_PEM_FILE", &self.scope_ledger_tls_ca_pem_file),
			("SCOPE_LEDGER_TLS_CERT_PEM_FILE", &self.scope_ledger_tls_cert_pem_file),
			("SCOPE_LEDGER_TLS_KEY_PEM_FILE", &self.scope_ledger_tls_key_pem_file),
		];
		let Some(addr) = &self.scope_ledger_addr else {
			let stray: Vec<&str> = files.iter().filter(|(_, v)| v.is_some()).map(|(name, _)| *name).collect();
			ensure!(
				stray.is_empty() && self.scope_ledger_service_token.is_none(),
				"SCOPE_LEDGER_ADDR is unset but its credentials are set ({stray:?}) — set the address or drop them"
			);
			return Ok(None);
		};
		ensure!(
			addr.starts_with("https://"),
			"SCOPE_LEDGER_ADDR {addr} must be https:// — banking's scope ledger speaks only mTLS"
		);
		let read = |(name, path): (&str, &Option<String>)| -> Result<String> {
			let path = path.as_deref().ok_or_else(|| color_eyre::eyre::eyre!("SCOPE_LEDGER_ADDR is set, so {name} is required"))?;
			std::fs::read_to_string(path).with_context(|| format!("failed to read {name} at {path}"))
		};
		let [ca, cert, key] = files;
		Ok(Some(ScopeLedgerConfig {
			addr: addr.clone(),
			ca_pem: read(ca)?,
			client_cert_pem: read(cert)?,
			client_key_pem: read(key)?,
			tls_domain: self.scope_ledger_tls_domain.clone(),
			service_token: self
				.scope_ledger_service_token
				.clone()
				.ok_or_else(|| color_eyre::eyre::eyre!("SCOPE_LEDGER_ADDR is set, so SCOPE_LEDGER_SERVICE_TOKEN is required"))?,
		}))
	}
}
