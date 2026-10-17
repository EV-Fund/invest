//! `invest-core window-import --file <export.jsonl> [--dry-run | --apply]` — step 3 of the
//! migration window (plan §6.5): banking's `window-export` file into invest. Like `import`, a
//! separate entry from `serve`: it dials what it needs, works and exits.
//!
//! `--dry-run` (the default) parses, checks and prints the Σ of units per allocation; it
//! reads no configuration, opens no connection and writes nothing. `--apply` writes.
//!
//! Exit codes: 0 clean, 1 the read-back of the ledger disagrees with the file, 2 the file was
//! refused or the run failed.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use color_eyre::eyre::{Context, Result};
use invest_core::{
	application::window_import::{
		self, AnchorHolder, is_digest,
		report::{Mode, Report},
	},
	config,
	infrastructure::{
		db,
		ledger::{self, TbLedger},
		tigerbeetle::TigerBeetle,
		window_import as apply,
	},
	ports::Ledger,
};

#[derive(Debug, Default, PartialEq)]
pub struct WindowImportArgs {
	pub file: PathBuf,
	pub apply: bool,
	/// SHA-256 the file must have; required with `--apply`.
	pub expect_digest: Option<String>,
	/// The `service_arb` holders to expect (whole units), as many as the anchor has.
	pub anchors: Vec<AnchorHolder>,
}

impl WindowImportArgs {
	const USAGE: &'static str = "usage: window-import --file <export.jsonl> [--anchor-holder <concierge_uuid>:<units>]... [--dry-run | --apply --expect-digest <sha256-hex>]";

	pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
		let (mut file, mut apply, mut dry_run, mut expect_digest, mut anchors) = (None, false, false, None, Vec::new());
		while let Some(arg) = args.next() {
			match arg.as_str() {
				"--dry-run" => dry_run = true,
				"--apply" => apply = true,
				"--expect-digest" => expect_digest = Some(args.next().ok_or_else(|| format!("--expect-digest needs a SHA-256; {}", Self::USAGE))?),
				"--anchor-holder" => {
					let raw = args.next().ok_or_else(|| format!("--anchor-holder needs <concierge_uuid>:<units>; {}", Self::USAGE))?;
					anchors.push(raw.parse::<AnchorHolder>()?);
				}
				"--file" => file = Some(args.next().ok_or_else(|| format!("--file needs a path; {}", Self::USAGE))?),
				other => return Err(format!("unknown argument {other}; {}", Self::USAGE)),
			}
		}
		if apply && dry_run {
			return Err(format!("--apply and --dry-run exclude each other; {}", Self::USAGE));
		}
		match (&expect_digest, apply) {
			(None, true) => return Err(format!("--apply needs --expect-digest, the SHA-256 the dry-run printed; {}", Self::USAGE)),
			(Some(_), false) => return Err(format!("--expect-digest only goes with --apply; {}", Self::USAGE)),
			(Some(d), true) if !is_digest(d) => return Err(format!("--expect-digest must be 64 lower-case hex characters; {}", Self::USAGE)),
			_ => {}
		}
		Ok(Self {
			expect_digest,
			anchors,
			file: file.ok_or_else(|| format!("window-import needs --file; {}", Self::USAGE))?.into(),
			apply,
		})
	}
}

pub fn run(args: WindowImportArgs) -> Result<ExitCode> {
	let bytes = std::fs::read(&args.file).with_context(|| format!("failed to read {}", args.file.display()))?;
	let refuse = |message: String| {
		eprintln!("{message}");
		Ok(ExitCode::from(2))
	};
	let plan = match window_import::plan(&bytes) {
		Ok(plan) => plan,
		Err(refusals) => return refuse(format!("{} is refused:\n{refusals}", args.file.display())),
	};

	let report: Report = if args.apply {
		let config = ev::settings::or_exit(config::AppConfig::from_env());
		let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("failed to build tokio runtime")?;
		let outcome = runtime.block_on(async {
			let pool = db::connect_sized(&config.database_url, 2).await.context("failed to connect to Postgres")?;
			let cluster_id: u128 = config.tigerbeetle_cluster_id.parse().context("TIGERBEETLE_CLUSTER_ID must be a u128")?;
			let tigerbeetle = Arc::new(TigerBeetle::connect(cluster_id, &config.tigerbeetle_address)?);
			let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
			ledger::probe(ledger.as_ref()).await.context("TigerBeetle did not answer")?;
			let guards = apply::Guards {
				expect_digest: args.expect_digest.as_deref().unwrap_or_default(),
				anchors: &args.anchors,
				own_cluster_id: &config.tigerbeetle_cluster_id,
			};
			Ok::<_, color_eyre::Report>(apply::apply(&pool, ledger.as_ref(), &plan, &guards).await)
		})?;
		match outcome {
			Ok(outcome) => {
				println!(
					"{}: {} unit transfers submitted",
					if outcome.rows_written { "registry rows written" } else { "registry rows were already there" },
					outcome.transfers_submitted
				);
				outcome.report
			}
			Err(err) => return refuse(format!("window-import failed: {err}")),
		}
	} else {
		Report::of_plan(&plan, Mode::DryRun, &args.anchors)
	};

	print!("{}", report.render());
	println!("{}", serde_json::to_string_pretty(&report)?);
	Ok(if report.ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

#[cfg(test)]
mod tests {
	use super::WindowImportArgs;

	fn parse(args: &[&str]) -> Result<WindowImportArgs, String> {
		WindowImportArgs::parse(args.iter().map(ToString::to_string))
	}

	#[test]
	fn a_dry_run_is_the_default_and_apply_must_be_asked_for() {
		assert!(!parse(&["--file", "x.jsonl"]).unwrap().apply);
		assert!(!parse(&["--file", "x.jsonl", "--dry-run"]).unwrap().apply);
		assert!(parse(&["--file", "x.jsonl", "--apply", "--expect-digest", &"a".repeat(64)]).unwrap().apply);
	}

	#[test]
	fn the_two_modes_and_unknown_arguments_are_refused() {
		assert!(parse(&["--file", "x", "--apply", "--dry-run"]).is_err());
		assert!(parse(&["--apply"]).is_err());
		assert!(parse(&["--file", "x", "--apply"]).is_err(), "apply needs the digest");
		assert!(parse(&["--file", "x", "--apply", "--expect-digest", "ABC"]).is_err());
		assert!(parse(&["--file", "x", "--expect-digest", &"a".repeat(64)]).is_err(), "a digest only goes with apply");
		assert!(parse(&["--file", "x", "--anchor-holder", "nope"]).is_err());
		assert!(parse(&["--file", "x", "--force"]).is_err());
	}
}
