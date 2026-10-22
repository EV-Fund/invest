//! An in-memory concierge for tests: the two pull streams relying parties read, the
//! catalog publication ([`InMemoryCatalogPublisher`]) and the approvals
//! ([`InMemoryApprovals`]), without concierge.
//!
//! It keeps what the engine leans on — each stream's own positions, oldest first, rows never
//! deleted, `next_position` as concierge computes it (the position a full page ends on; the
//! stream's head when the page did not fill; `after` unchanged when nothing was scanned) —
//! and none of concierge's filtering: every row pushed is this client's to see.
//!
//! A test steers it with [`InMemoryConcierge::push_user_unknown`] /
//! [`push_scope_unknown`](InMemoryConcierge::push_scope_unknown) (a kind this build cannot
//! name), [`upgrade_user`](InMemoryConcierge::upgrade_user) (the same row, read by a build
//! that can) and [`outage`](InMemoryConcierge::outage). Cloning shares the streams, so the
//! test keeps a handle on what the engine pulls.
//!
//! Never wired into a running server.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use evconcierge_projection::{EventSource, Page, Pulled, ScopeEvent, SourceError, UnknownEvent, UserEvent};
use invest_domain::authz::PermissionCatalog;

use crate::ports::{
	approvals::{ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal, payload_hash},
	catalog::{CatalogPublisher, KindLevel, PublishError, ScopeCatalog, ScopeCatalogPublisher},
};

#[derive(Clone, Default)]
pub struct InMemoryConcierge {
	state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
	users: Vec<(i64, Pulled<UserEvent>)>,
	scopes: Vec<(i64, Pulled<ScopeEvent>)>,
	/// While set, every pull answers UNAVAILABLE.
	down: bool,
}

impl InMemoryConcierge {
	pub fn new() -> Self {
		Self::default()
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, State> {
		// A test that panicked while holding the lock already failed; the state it left is
		// still the best answer for the assertions that follow.
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Append a lifecycle row; returns its position.
	pub fn push_user(&self, event: UserEvent) -> i64 {
		let mut state = self.lock();
		let position = next(&state.users);
		state.users.push((position, Pulled::Known(event)));
		position
	}

	/// Append a lifecycle row of a kind this build cannot name, for `auth_subject`.
	pub fn push_user_unknown(&self, event_id: &str, auth_subject: &str, kind: i32) -> i64 {
		let mut state = self.lock();
		let position = next(&state.users);
		state.users.push((
			position,
			Pulled::Unknown(UnknownEvent {
				event_id: event_id.to_owned(),
				// The lifecycle wire carries no position; the engine learns it by re-pulling.
				position: None,
				key: format!("user:{auth_subject}"),
				kind,
				why: "a lifecycle kind this build cannot name",
			}),
		));
		position
	}

	/// Replace the row at `position` with `event` — what a build that can read the kind sees
	/// when it re-pulls the same row.
	pub fn upgrade_user(&self, position: i64, event: UserEvent) {
		let mut state = self.lock();
		let row = state.users.iter_mut().find(|(p, _)| *p == position).unwrap_or_else(|| panic!("no lifecycle row at {position}"));
		row.1 = Pulled::Known(event);
	}

	/// Append a scope row; its `position` is assigned here and returned.
	pub fn push_scope(&self, mut event: ScopeEvent) -> i64 {
		let mut state = self.lock();
		let position = next(&state.scopes);
		event.position = position;
		state.scopes.push((position, Pulled::Known(event)));
		position
	}

	/// Append a scope row of a kind this build cannot name, about `scope`.
	pub fn push_scope_unknown(&self, event_id: &str, scope: &str, kind: i32) -> i64 {
		let mut state = self.lock();
		let position = next(&state.scopes);
		state.scopes.push((
			position,
			Pulled::Unknown(UnknownEvent {
				event_id: event_id.to_owned(),
				position: Some(position),
				key: format!("scope:{scope}"),
				kind,
				why: "a scope event kind this build cannot name",
			}),
		));
		position
	}

	/// Whether every pull answers UNAVAILABLE from now on.
	pub fn outage(&self, down: bool) {
		self.lock().down = down;
	}

	fn page<T: Clone>(rows: &[(i64, Pulled<T>)], after: i64, limit: u32, down: bool) -> Result<Page<Pulled<T>>, SourceError> {
		if down {
			return Err(SourceError::Unavailable("concierge is down (fake)".into()));
		}
		// concierge serves a pull without a limit one row; so does the fake.
		let limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
		let page: Vec<&(i64, Pulled<T>)> = rows.iter().filter(|(p, _)| *p > after).take(limit).collect();
		let next_position = if page.len() == limit {
			page.last().map_or(after, |(p, _)| *p)
		} else {
			rows.last().map_or(after, |(p, _)| (*p).max(after))
		};
		Ok(Page {
			events: page.into_iter().map(|(_, e)| e.clone()).collect(),
			next_position,
		})
	}
}

fn next<T>(rows: &[(i64, T)]) -> i64 {
	rows.last().map_or(1, |(p, _)| p + 1)
}

#[async_trait]
impl EventSource for InMemoryConcierge {
	async fn pull_user_events(&self, after: i64, limit: u32) -> Result<Page<Pulled<UserEvent>>, SourceError> {
		let state = self.lock();
		Self::page(&state.users, after, limit, state.down)
	}

	async fn pull_scope_events(&self, after: i64, limit: u32) -> Result<Page<Pulled<ScopeEvent>>, SourceError> {
		let state = self.lock();
		Self::page(&state.scopes, after, limit, state.down)
	}
}

/// An in-memory catalog publisher: records what it was asked to publish, and answers each
/// call with the next scripted outcome (then `Ok` once the script runs out). One type for
/// both catalogs, so their fakes cannot drift apart.
pub struct ScriptedPublisher<C> {
	state: Arc<Mutex<PublisherState<C>>>,
}

struct PublisherState<C> {
	script: std::collections::VecDeque<Result<(), PublishError>>,
	attempts: u32,
	published: Vec<(C, u64)>,
}

// Manual impls: a derive would demand `C: Clone`/`Default` of the handle, not of the catalog.
impl<C> Clone for ScriptedPublisher<C> {
	fn clone(&self) -> Self {
		Self { state: Arc::clone(&self.state) }
	}
}

impl<C> Default for ScriptedPublisher<C> {
	fn default() -> Self {
		Self {
			state: Arc::new(Mutex::new(PublisherState {
				script: std::collections::VecDeque::new(),
				attempts: 0,
				published: Vec::new(),
			})),
		}
	}
}

impl<C: Clone> ScriptedPublisher<C> {
	pub fn new() -> Self {
		Self::default()
	}

	/// Answer the next calls with `outcomes`, in order.
	pub fn script(&self, outcomes: impl IntoIterator<Item = Result<(), PublishError>>) {
		self.lock().script.extend(outcomes);
	}

	/// Every call made, successful or not.
	pub fn attempts(&self) -> u32 {
		self.lock().attempts
	}

	/// What was accepted, in order.
	pub fn published(&self) -> Vec<(C, u64)> {
		self.lock().published.clone()
	}

	fn record(&self, catalog: &C, version: u64) -> Result<(), PublishError> {
		let mut state = self.lock();
		state.attempts += 1;
		let outcome = state.script.pop_front().unwrap_or(Ok(()));
		if outcome.is_ok() {
			state.published.push((catalog.clone(), version));
		}
		outcome
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, PublisherState<C>> {
		// See `InMemoryConcierge::lock`.
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

pub type InMemoryCatalogPublisher = ScriptedPublisher<PermissionCatalog>;
pub type InMemoryScopeCatalogPublisher = ScriptedPublisher<ScopeCatalog>;

#[async_trait]
impl CatalogPublisher for InMemoryCatalogPublisher {
	async fn publish(&self, catalog: &PermissionCatalog, version: u64) -> Result<(), PublishError> {
		self.record(catalog, version)
	}
}

#[async_trait]
impl ScopeCatalogPublisher for InMemoryScopeCatalogPublisher {
	async fn publish(&self, catalog: &ScopeCatalog, version: u64) -> Result<(), PublishError> {
		self.record(catalog, version)
	}
}

/// concierge's approvals, in memory: enough of §5.4 to drive invest's proposer and executor
/// — `Propose` deduped per key and hashed as concierge hashes, the state a test sets,
/// `PullApproved` serving every passed proposal until its outcome is reported, and a
/// withdrawal while open or passed. A test steers it with [`InMemoryApprovals::set_state`],
/// [`InMemoryApprovals::tamper_hash`], [`InMemoryApprovals::unreport`],
/// [`InMemoryApprovals::hold_back`], [`InMemoryApprovals::refuse_proposals`] and
/// [`InMemoryApprovals::outage`].
#[derive(Clone, Default)]
pub struct InMemoryApprovals {
	state: Arc<Mutex<ApprovalsState>>,
}

#[derive(Default)]
struct ApprovalsState {
	proposals: Vec<StoredProposal>,
	reports: Vec<(String, ApprovalOutcome, String)>,
	withdrawals: Vec<String>,
	refuse: Option<String>,
	down: bool,
}

struct StoredProposal {
	view: ApprovalView,
	proposal: Proposal,
	reported: bool,
	/// Passed but inside its veto delay: not served by a pull yet.
	held_back: bool,
}

impl InMemoryApprovals {
	pub fn new() -> Self {
		Self::default()
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, ApprovalsState> {
		// A test that panicked while holding the lock already failed.
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Every proposal opened, in order.
	pub fn proposals(&self) -> Vec<(ApprovalView, Proposal)> {
		self.lock().proposals.iter().map(|p| (p.view.clone(), p.proposal.clone())).collect()
	}

	/// Every outcome reported, in order: approval id, outcome, detail.
	pub fn reports(&self) -> Vec<(String, ApprovalOutcome, String)> {
		self.lock().reports.clone()
	}

	/// Every approval id withdrawn, in order.
	pub fn withdrawals(&self) -> Vec<String> {
		self.lock().withdrawals.clone()
	}

	/// The owners (or the clock) move a proposal to `state`, with the veto or void reason.
	pub fn set_state(&self, approval_id: &str, state: ApprovalState, note: &str) {
		let mut guard = self.lock();
		let stored = guard.proposals.iter_mut().find(|p| p.view.id == approval_id).expect("no such proposal");
		stored.view.state = state;
		stored.view.note = note.to_owned();
	}

	/// Make concierge hold the proposal at another level than the one it resolved.
	pub fn set_level(&self, approval_id: &str, level: Option<KindLevel>) {
		let mut guard = self.lock();
		let stored = guard.proposals.iter_mut().find(|p| p.view.id == approval_id).expect("no such proposal");
		stored.view.level = level;
	}

	/// Make concierge hold another payload hash than the one it computed — a proposal whose
	/// terms are not the change's.
	pub fn tamper_hash(&self, approval_id: &str) {
		let mut guard = self.lock();
		let stored = guard.proposals.iter_mut().find(|p| p.view.id == approval_id).expect("no such proposal");
		stored.view.payload_hash = "0".repeat(64);
	}

	/// Forget that an outcome was reported — a report concierge lost — so a pull serves the
	/// proposal again.
	pub fn unreport(&self, approval_id: &str) {
		let mut guard = self.lock();
		let stored = guard.proposals.iter_mut().find(|p| p.view.id == approval_id).expect("no such proposal");
		stored.reported = false;
	}

	/// Keep a passed proposal out of the pull, as its veto delay does.
	pub fn hold_back(&self, approval_id: &str, held: bool) {
		let mut guard = self.lock();
		let stored = guard.proposals.iter_mut().find(|p| p.view.id == approval_id).expect("no such proposal");
		stored.held_back = held;
	}

	/// While set, `Propose` is refused with this reason.
	pub fn refuse_proposals(&self, why: Option<&str>) {
		self.lock().refuse = why.map(str::to_owned);
	}

	/// While set, every call answers UNAVAILABLE.
	pub fn outage(&self, down: bool) {
		self.lock().down = down;
	}

	fn up(&self) -> Result<std::sync::MutexGuard<'_, ApprovalsState>, ApprovalsError> {
		let guard = self.lock();
		if guard.down {
			return Err(ApprovalsError::Unavailable("concierge is down (fake)".into()));
		}
		Ok(guard)
	}
}

#[async_trait]
impl Approvals for InMemoryApprovals {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError> {
		let mut guard = self.up()?;
		if let Some(why) = &guard.refuse {
			return Err(ApprovalsError::Refused(why.clone()));
		}
		if let Some(existing) = guard.proposals.iter().find(|p| p.proposal.dedupe_key == proposal.dedupe_key) {
			return Ok(existing.view.clone());
		}
		let view = ApprovalView {
			id: uuid::Uuid::new_v4().to_string(),
			kind: proposal.kind.to_owned(),
			scope: proposal.scope.to_owned(),
			subject_ref: proposal.subject_ref.clone(),
			state: ApprovalState::Open,
			// What concierge resolves `invest:fee_policy` to; a test moves it with `set_level`.
			level: Some(KindLevel::Platform),
			payload_hash: payload_hash(proposal.kind, proposal.scope, &proposal.subject_ref, &proposal.fields),
			note: String::new(),
		};
		guard.proposals.push(StoredProposal {
			view: view.clone(),
			proposal: proposal.clone(),
			reported: false,
			held_back: false,
		});
		Ok(view)
	}

	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError> {
		let guard = self.up()?;
		guard
			.proposals
			.iter()
			.find(|p| p.view.id == approval_id)
			.map(|p| p.view.clone())
			.ok_or_else(|| ApprovalsError::Refused(format!("NotFound: no approval {approval_id}")))
	}

	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError> {
		let guard = self.up()?;
		Ok(guard
			.proposals
			.iter()
			.filter(|p| p.view.state == ApprovalState::Passed && !p.reported && !p.held_back)
			.take(usize::try_from(limit).unwrap_or(usize::MAX))
			.map(|p| p.view.clone())
			.collect())
	}

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError> {
		let mut guard = self.up()?;
		let stored = guard
			.proposals
			.iter_mut()
			.find(|p| p.view.id == approval_id)
			.ok_or_else(|| ApprovalsError::Refused(format!("NotFound: no approval {approval_id}")))?;
		stored.reported = true;
		stored.view.state = match outcome {
			ApprovalOutcome::Executed => ApprovalState::Executed,
			ApprovalOutcome::Failed => ApprovalState::ExecutionFailed,
			ApprovalOutcome::Void => ApprovalState::Void,
		};
		guard.reports.push((approval_id.to_owned(), outcome, detail.to_owned()));
		Ok(())
	}

	async fn withdraw(&self, approval_id: &str, _reason: &str) -> Result<ApprovalView, ApprovalsError> {
		let mut guard = self.up()?;
		let stored = guard
			.proposals
			.iter_mut()
			.find(|p| p.view.id == approval_id)
			.ok_or_else(|| ApprovalsError::Refused(format!("NotFound: no approval {approval_id}")))?;
		if !stored.view.state.is_live() {
			return Err(ApprovalsError::Refused(format!("FailedPrecondition: the approval is {}", stored.view.state.as_str())));
		}
		stored.view.state = ApprovalState::Cancelled;
		let view = stored.view.clone();
		guard.withdrawals.push(approval_id.to_owned());
		Ok(view)
	}
}
