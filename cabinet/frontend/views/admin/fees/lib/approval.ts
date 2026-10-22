// Words for the owners' decision on a fee change, as concierge's approvals report it.
// Every helper takes the caller's `t`, like `./format.ts`.

import type { Locale, Translate } from "@evinvest/i18n";

import type { FeePolicyChange } from "@/shared/contracts/admin";
import { formatMoment, hasStamp } from "@/shared/lib/datetime";
import { wordFor } from "@/shared/lib/wire-words";

const labels = (t: Translate): Readonly<Record<string, string>> => ({
  open: t("admin.fees.approval.state.open", "Voting"),
  passed: t("admin.fees.approval.state.passed", "Carried"),
  executed: t("admin.fees.approval.state.executed", "Carried out"),
  execution_failed: t("admin.fees.approval.state.execution_failed", "Could not be carried out"),
  rejected: t("admin.fees.approval.state.rejected", "Rejected"),
  vetoed: t("admin.fees.approval.state.vetoed", "Vetoed"),
  expired: t("admin.fees.approval.state.expired", "Expired"),
  cancelled: t("admin.fees.approval.state.cancelled", "Withdrawn"),
  void: t("admin.fees.approval.state.void", "Voided"),
});

/** The short name for a state pill. An unknown wire word renders as itself. */
export function approvalStateLabel(change: Pick<FeePolicyChange, "approval_state" | "consilium_id">, t: Translate): string {
  const state = change.approval_state;
  if (state === null) return change.consilium_id ? t("admin.fees.approval.state.unknown", "State not known") : t("admin.fees.approval.state.unproposed", "Not proposed yet");
  return wordFor(labels(t), state) ?? state;
}

const lines = (t: Translate): Readonly<Record<string, string>> => ({
  open: t("admin.fees.approval.line.openRoster", "The owners are voting. The change passes when more than half of the owners on its vote approve it. The person who asked for it is not on that vote."),
  executed: t("admin.fees.approval.line.executed", "The owners carried the change, and invest scheduled it."),
  execution_failed: t("admin.fees.approval.line.execution_failed", "The owners carried the change, but invest could not schedule it. The change is closed."),
  rejected: t("admin.fees.approval.line.rejected", "The owners rejected the change. The fund keeps its current terms."),
  vetoed: t("admin.fees.approval.line.vetoed", "An owner vetoed the change. The fund keeps its current terms."),
  expired: t("admin.fees.approval.line.expired", "The owners did not decide within 72 hours, so the proposal expired."),
  cancelled: t("admin.fees.approval.line.cancelled", "The proposal was withdrawn, so the votes given on it no longer count."),
  void: t("admin.fees.approval.line.void", "concierge voided the proposal, so the votes given on it no longer count."),
});

/** One plain sentence about where the owners' decision stands, or `null` for a state this
 *  build has no sentence for (the pill still names it). */
export function approvalStateLine(change: FeePolicyChange, t: Translate, locale: Locale): string | null {
  const state = change.approval_state;
  // An id without a state is a change imported from banking, whose consilium invest never
  // read: nothing true can be said about it beyond the pill.
  if (state === null && change.consilium_id) return null;
  if (state === null) {
    // No approval id yet: concierge was out of reach when the change was asked for, and the
    // invest approvals round (every 30 s) proposes it again.
    return t("admin.fees.approval.line.unproposed", "concierge could not be reached when you asked. invest proposes the change to the owners again within a minute.");
  }
  if (state === "passed") {
    if (change.state !== "awaiting_consilium") return t("admin.fees.approval.line.passed", "The owners carried the change, and invest scheduled it.");
    // The end of the veto window is known only once invest has read the passed proposal;
    // until then the 24 h rule is all that can be said.
    const until = change.approval_executable_at;
    return until && hasStamp(until)
      ? t("admin.fees.approval.line.passedUntil", "The owners carried the change. Owners can veto it until {at}; after that, invest schedules it.", { at: formatMoment(until, locale) })
      : t("admin.fees.approval.line.passedAwaiting", "The owners carried the change. Any owner can still veto it during the 24-hour delay that follows; after that, invest schedules it.");
  }
  return wordFor(lines(t), state) ?? null;
}

/** Token classes for the approval pill, in the same tones as the change's own pill. */
export function approvalStateTone(state: string | null): string {
  if (state === "executed") return "text-positive";
  if (state === null || state === "open" || state === "passed") return "text-accent-debug";
  if (state === "rejected" || state === "vetoed" || state === "execution_failed") return "text-accent-error";
  return "text-ink-soft";
}
