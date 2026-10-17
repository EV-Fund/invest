//! The migration-window import (plan §6.5, step 3): banking's `window-export` file, read
//! with the structs that wrote it (`banking_domain::window_export`), checked, and turned into
//! what invest stores. Pure — no database, no ledger — so `--dry-run` is exactly [`plan()`]
//! plus [`report::Report::of_plan`]. Applying the plan is the infrastructure adapter's,
//! `infrastructure::window_import`.
//!
//! Not part of the old `import` command (`application::import`, invest's own file format):
//! that one reads a file nothing writes; this one reads the format banking's exporter ships.

mod plan;
pub mod report;

pub use plan::{Anchor, AnchorHolder, Holding, Plan, Refusals, SA_SERVICE, Totals, digest_of, is_digest, plan, scope_of};
