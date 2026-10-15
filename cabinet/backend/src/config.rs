use std::net::SocketAddr;

ev::settings! {
	/// The cabinet BFF's configuration, read from the environment only (`dotenvy` loads
	/// `.env` first in dev; prod values ride the container env — the image's baked topology
	/// contract plus gitops' k8s Secret `envFrom`). Every missing or invalid variable is
	/// reported in one aggregate error before anything binds, and an empty string counts as
	/// unset.
	pub struct AppConfig {
		/// HTTP listener — where the cabinet's `/cabinet/api/*` and `/cabinet/auth/*` arrive.
		/// Loopback by default: the request credential is a cookie, so the BFF sits behind
		/// the same-origin reverse proxy; production binds wider behind the network policy.
		#[env("CABINET_BACKEND_BIND")]
		bind: SocketAddr = "127.0.0.1:50072",
		/// The `invest` database. The BFF owns the `cabinet` schema in it (sessions, sign-in
		/// state) and migrates it on boot; it never touches invest-core's tables.
		database_url: String,
		db_max_connections: u32 = "5",
		/// invest-core's gRPC (`http://host:50071`), called with the signed-in user's access
		/// token.
		invest_core_grpc_addr: String,
		/// concierge's gRPC (`http://host:55670`): `ExchangeCode`, `RefreshClientToken`, `GetMe`.
		concierge_grpc_addr: String,
		/// The relying-party client the cabinet signs in as.
		concierge_client_id: String = "invest",
		/// That client's secret — the value concierge holds as `RP_CLIENT_SECRET_INVEST`.
		#[secret]
		concierge_client_secret: String,
		/// The public origin of concierge's web surface; the browser is sent to
		/// `<origin>/api/auth/authorize`.
		concierge_public_origin: String = "https://evinvest.ltd",
		/// This cabinet's public origin (`https://invest.evinvest.ltd`): the redirect URI is
		/// `<origin>/cabinet/auth/callback`, byte for byte as concierge registered it, and the
		/// only `Origin` a mutation or the book socket is accepted from.
		cabinet_public_origin: String,
		/// Explicit override for cookie hardening; unset ⇒ inferred from the cabinet's origin
		/// (`__Host-` + `Secure` need HTTPS, which dev lacks). See [`Self::cookie_secure`].
		auth_cookie_secure: Option<bool>,
		app_env: String,
		/// Unset ⇒ error monitoring is a silent no-op (`ev::error_monitoring::init` returns
		/// no guard). Optional even in production: invest's own Sentry project is not created
		/// yet (EV-Fund/invest#18), so the DSN is added to the deploy env later and then just
		/// starts working.
		sentry_dsn: Option<String>,
	}
}

impl AppConfig {
	/// Whether cookies are `__Host-`-prefixed and `Secure`. Follows the scheme the cabinet is
	/// served on, not `APP_ENV`: an https stand left on `development` must still lock its
	/// cookies to its host (security review F5).
	pub fn cookie_secure(&self) -> bool {
		self.auth_cookie_secure.unwrap_or_else(|| self.cabinet_origin().starts_with("https://"))
	}

	/// What the per-field checks cannot see: an https cabinet told to hand out cookies a
	/// sibling subdomain of evinvest.ltd could set or shadow. `main` exits 78 on it.
	pub fn validate(&self) -> Result<(), String> {
		if self.cabinet_origin().starts_with("https://") && !self.cookie_secure() {
			return Err(format!(
				"AUTH_COOKIE_SECURE=false with CABINET_PUBLIC_ORIGIN={}: an https cabinet must use __Host- Secure cookies",
				self.cabinet_origin()
			));
		}
		Ok(())
	}

	/// The origin without a trailing slash — what a browser sends as `Origin`.
	pub fn cabinet_origin(&self) -> &str {
		self.cabinet_public_origin.trim_end_matches('/')
	}

	/// Where concierge sends the browser back with the code. Must equal the URI registered
	/// for the client (concierge `runner/migrations/0031_scopes.up.sql`).
	pub fn redirect_uri(&self) -> String {
		format!("{}{}", self.cabinet_origin(), crate::routes::CALLBACK_PATH)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	fn minimal_env() -> HashMap<String, String> {
		[
			("DATABASE_URL", "postgres://postgres@127.0.0.1:5432/invest"),
			("INVEST_CORE_GRPC_ADDR", "http://127.0.0.1:50071"),
			("CONCIERGE_GRPC_ADDR", "http://127.0.0.1:55670"),
			("CONCIERGE_CLIENT_SECRET", "test-secret"),
			("CABINET_PUBLIC_ORIGIN", "http://localhost:3000"),
			("APP_ENV", "development"),
		]
		.into_iter()
		.map(|(k, v)| (k.to_string(), v.to_string()))
		.collect()
	}

	#[test]
	fn default_bind_is_loopback() {
		let config = AppConfig::from_source(|var| minimal_env().get(var).cloned()).expect("minimal env loads");
		assert!(config.bind.ip().is_loopback(), "the BFF must default to loopback, not all interfaces");
		assert_eq!(config.bind.port(), 50072);
	}

	/// The env surface IS the deploy contract (the flake image env and gitops manifests use
	/// exactly these names) — a rename here must be deliberate.
	#[test]
	fn env_surface_matches_the_deploy_contract() {
		assert_eq!(
			AppConfig::var_names(),
			vec![
				"CABINET_BACKEND_BIND",
				"DATABASE_URL",
				"DB_MAX_CONNECTIONS",
				"INVEST_CORE_GRPC_ADDR",
				"CONCIERGE_GRPC_ADDR",
				"CONCIERGE_CLIENT_ID",
				"CONCIERGE_CLIENT_SECRET",
				"CONCIERGE_PUBLIC_ORIGIN",
				"CABINET_PUBLIC_ORIGIN",
				"AUTH_COOKIE_SECURE",
				"APP_ENV",
				"SENTRY_DSN",
			]
		);
	}

	/// The gitops preflight diffs the cluster Secret against `--print-required-vars`, so
	/// listing `SENTRY_DSN` there would block deploys until invest's Sentry project exists.
	#[test]
	fn error_monitoring_is_optional_in_production() {
		assert!(!AppConfig::required_var_names("production").contains(&"SENTRY_DSN".to_string()));
		let config = load(&[("APP_ENV", "production")]);
		assert_eq!(config.sentry_dsn, None);
	}

	fn load(pairs: &[(&str, &str)]) -> AppConfig {
		let mut env = minimal_env();
		for (k, v) in pairs {
			env.insert((*k).into(), (*v).into());
		}
		AppConfig::from_source(|var| env.get(var).cloned()).expect("env loads")
	}

	/// An https cabinet gets `__Host-` + `Secure` cookies whatever `APP_ENV` says: a staging
	/// stand left on `development` must not hand out cookies a sibling subdomain can shadow
	/// (security review F5).
	#[test]
	fn cookie_hardening_follows_the_public_origin() {
		assert!(load(&[("CABINET_PUBLIC_ORIGIN", "https://invest.evinvest.ltd")]).cookie_secure());
		assert!(!load(&[("CABINET_PUBLIC_ORIGIN", "http://localhost:3000")]).cookie_secure());
		assert!(!load(&[("CABINET_PUBLIC_ORIGIN", "http://localhost:3000"), ("APP_ENV", "production")]).cookie_secure());
	}

	#[test]
	fn an_https_cabinet_with_insecure_cookies_refuses_to_boot() {
		let config = load(&[("CABINET_PUBLIC_ORIGIN", "https://invest.evinvest.ltd"), ("AUTH_COOKIE_SECURE", "false")]);
		assert!(config.validate().is_err());
		assert!(load(&[("CABINET_PUBLIC_ORIGIN", "https://invest.evinvest.ltd")]).validate().is_ok());
		assert!(
			load(&[("AUTH_COOKIE_SECURE", "true")]).validate().is_ok(),
			"secure cookies over http are the browser's problem, not a leak"
		);
	}

	/// The redirect URI is the registered one byte for byte, whatever the operator typed
	/// after the origin.
	#[test]
	fn the_redirect_uri_is_the_registered_callback() {
		let mut env = minimal_env();
		env.insert("CABINET_PUBLIC_ORIGIN".into(), "https://invest.evinvest.ltd/".into());
		let config = AppConfig::from_source(|var| env.get(var).cloned()).expect("env loads");
		assert_eq!(config.redirect_uri(), "https://invest.evinvest.ltd/cabinet/auth/callback");
		assert_eq!(config.cabinet_origin(), "https://invest.evinvest.ltd");
	}
}
