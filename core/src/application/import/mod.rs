//! The data-wave import (plan §6.5, step 3): the file format and its validation
//! ([`format`]) and the reconciliation arithmetic ([`reconcile`]), both pure. Writing the
//! file's rows and units is the infrastructure adapter's, `infrastructure::import`.

pub mod format;
pub mod reconcile;

use format::ImportFile;
use reconcile::{Anchor, Mode, Report};

/// `--dry-run`: everything that can be known from the file alone. No Postgres, no ledger.
pub fn dry_run(file: &ImportFile, anchors: &[Anchor]) -> Report {
	reconcile::reconcile(file, &reconcile::balances_from_file(file), anchors, Mode::DryRun)
}
