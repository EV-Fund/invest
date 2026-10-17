//! `invest-core import --file <path> [--dry-run] [--anchor <slug>=<user>:<units>]...` — the
//! one-shot data-wave importer. A separate entry from `serve`: it dials what it needs, does
//! its work and exits, and never starts the server, the relay or the projections.
//!
//! Exit codes: 0 clean, 1 the reconciliation found a mismatch, 2 the run was refused or failed.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use color_eyre::eyre::{Context, Result};
use invest_core::{
	application::import::{self, format, reconcile::Anchor},
	config,
	infrastructure::{
		db, import as apply,
		ledger::{self, TbLedger},
		tigerbeetle::TigerBeetle,
	},
	ports::Ledger,
};

#[derive(Debug, Default, PartialEq)]
pub struct ImportArgs {
	pub file: PathBuf,
	pub dry_run: bool,
	pub anchors: Vec<String>,
}

impl ImportArgs {
	pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
		let mut out = Self::default();
		let mut file = None;
		while let Some(arg) = args.next() {
			match arg.as_str() {
				"--dry-run" => out.dry_run = true,
				"--file" => file = Some(args.next().ok_or("--file needs a path")?),
				"--anchor" => out.anchors.push(args.next().ok_or("--anchor needs <slug>=<user>:<units>")?),
				other =>
					return Err(format!(
						"unknown argument {other}; usage: import --file <path.jsonl> [--dry-run] [--anchor <slug>=<user>:<units>]..."
					)),
			}
		}
		out.file = file.ok_or("import needs --file <path.jsonl>")?.into();
		Ok(out)
	}
}

pub fn run(args: ImportArgs) -> Result<ExitCode> {
	let text = std::fs::read_to_string(&args.file).with_context(|| format!("failed to read {}", args.file.display()))?;
	let refuse = |message: String| {
		eprintln!("{message}");
		Ok(ExitCode::from(2))
	};
	let anchors = match args.anchors.iter().map(|a| a.parse::<Anchor>()).collect::<Result<Vec<_>, _>>() {
		Ok(anchors) => anchors,
		Err(message) => return refuse(message),
	};
	let file = match format::parse(&text) {
		Ok(file) => file,
		Err(errors) => return refuse(format!("{} is not a valid data-wave file:\n{errors}", args.file.display())),
	};

	let report = if args.dry_run {
		// Deliberately before any config read: a dry run needs neither the database nor the ledger.
		import::dry_run(&file, &anchors)
	} else {
		let config = ev::settings::or_exit(config::AppConfig::from_env());
		let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("failed to build tokio runtime")?;
		let outcome = runtime.block_on(async {
			let pool = db::connect_sized(&config.database_url, 2).await.context("failed to connect to Postgres")?;
			let cluster_id: u128 = config.tigerbeetle_cluster_id.parse().context("TIGERBEETLE_CLUSTER_ID must be a u128")?;
			let tigerbeetle = Arc::new(TigerBeetle::connect(cluster_id, &config.tigerbeetle_address)?);
			let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
			ledger::probe(ledger.as_ref()).await.context("TigerBeetle did not answer")?;
			Ok::<_, color_eyre::Report>(apply::run(&pool, ledger.as_ref(), &file, &anchors).await)
		})?;
		match outcome {
			Ok(outcome) => {
				if !outcome.written {
					println!("NOTHING WRITTEN: the file does not reconcile with itself");
				} else {
					println!("imported {} records, {} already imported", outcome.imported, outcome.already_imported);
				}
				outcome.report
			}
			Err(err) => return refuse(format!("import failed: {err}")),
		}
	};

	print!("{}", report.render());
	println!("{}", serde_json::to_string_pretty(&report)?);
	Ok(if report.ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}
