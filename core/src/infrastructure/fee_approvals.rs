//! The approvals round as a background worker: [`fee_approvals::round`] every
//! [`ROUND_INTERVAL`] until shutdown. It holds no lock: every step of a round is idempotent,
//! so a second replica running its own costs a duplicate call to concierge, never a second
//! scheduling.

use std::sync::Arc;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
	application::fee_approvals::{self, ApprovalRound, FeeApprovalPorts, ROUND_INTERVAL},
	infrastructure::now_unix,
	ports::{
		approvals::Approvals,
		fees::{FeePolicies, FeePolicyChanges},
	},
};

pub struct FeeApprovalsExecutor {
	approvals: Arc<dyn Approvals>,
	changes: Arc<dyn FeePolicyChanges>,
	policies: Arc<dyn FeePolicies>,
}

impl FeeApprovalsExecutor {
	pub fn new(approvals: Arc<dyn Approvals>, changes: Arc<dyn FeePolicyChanges>, policies: Arc<dyn FeePolicies>) -> Self {
		Self { approvals, changes, policies }
	}

	pub async fn run(self, shutdown: CancellationToken) {
		let mut ticks = tokio::time::interval(ROUND_INTERVAL);
		ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
		loop {
			tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				_ = ticks.tick() => {},
			}
			let ports = FeeApprovalPorts {
				approvals: self.approvals.as_ref(),
				changes: self.changes.as_ref(),
				policies: self.policies.as_ref(),
			};
			match fee_approvals::round(&ports, now_unix()).await {
				Ok(round) if round == ApprovalRound::default() => {}
				Ok(round) => info!(?round, "fee approvals: round done"),
				// The database is away: the next round tries again from the same rows.
				Err(err) => error!("fee approvals: the round failed, will retry: {err}"),
			}
		}
	}
}
