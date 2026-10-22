"use client";

// The change on its way for one product, and the one thing an operator can do to it.
//
// Cancelling is not the reversible click a single ghost button makes it look like: a
// change still awaiting the owners withdraws its proposal in concierge's approvals, and
// every vote given on it stops counting. So the button opens a second step in place — no
// `window.confirm`, whose text no catalogue can translate.

import { useEffect, useRef, useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Alert, AlertDescription, Badge, Button, Card, CardContent, Spinner } from "@evinvest/uikit";

import { cancelFeePolicyChange } from "@/entities/admin/api/admin-client";
import { FeeTermsDiff } from "@/entities/fund/ui/fee-terms-diff";
import type { FeePolicy, FeePolicyChange } from "@/shared/contracts/admin";
import { errorMessage, RequestError } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { cn } from "@/shared/lib/cn";
import { formatMoment } from "@/shared/lib/datetime";
import { stripTransportPrefix } from "@/shared/lib/hub-refusal";
import { revalidateTag } from "@/shared/lib/resource";
import { changeStateLabel, changeStateTone } from "@/views/admin/fees/lib/format";
import { receiptKind } from "@/views/admin/fees/lib/receipt";
import { ApprovalStatus } from "@/views/admin/fees/ui/approval-status";
import { NoticeWaiver } from "@/views/admin/fees/ui/notice-waiver";
import { ago } from "@/views/admin/lib/format";

/** `current` is the terms in force, `null` when the fund charges nothing yet: the diff's
 *  "Now" column. A history row keeps no "from" terms, so only the pending card has them. */
export function PendingCard({ change, current, onCancelled }: { change: FeePolicyChange; current: FeePolicy | null; onCancelled: () => void }) {
  const t = useT();
  const locale = useLocale();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const awaiting = change.state === "awaiting_consilium";
  const owners = change.requirement === "owner_consilium";
  // A product nobody holds binds at the request itself and the card lives for the minute
  // until the sweeper applies it — "holders have been notified" would name nobody.
  const [kind] = useState(() => receiptKind(change, Math.floor(Date.now() / 1000)));
  // The button that opened the confirmation is gone the moment it opens, and focus would
  // fall to `body`. It lands on the SAFE answer instead: Enter from there backs out, and a
  // keyboard user has to aim at the destructive one deliberately.
  const keepRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (confirming) keepRef.current?.focus();
  }, [confirming]);

  async function cancel() {
    setBusy(true);
    setProblem(null);
    try {
      await cancelFeePolicyChange({ service: change.service, change_id: change.id });
      // The policy loses its `pending`, the history row turns `cancelled`, and the form
      // beneath unblocks — all readers of the tag.
      revalidateTag(TAG.adminFees);
      setConfirming(false);
      onCancelled();
    } catch (e) {
      // A 409 — "the change is active", a colleague cancelled it first — says this card is
      // about a change that no longer is what it shows. The same reread that answers a
      // success brings in what it became (banking#326): the card goes, the history row
      // turns. The sentence is shown meanwhile, without the BFF's variant prefix on it.
      if (e instanceof RequestError && e.status === 409) revalidateTag(TAG.adminFees);
      setProblem(stripTransportPrefix(errorMessage(e, t)));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className="h-fit border-primary-ink/40">
      <CardContent className="space-y-3 py-6">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <p className="text-sm font-semibold">{t("admin.fees.pendingTitle", "Change on its way")}</p>
          <Badge variant="outline" className={cn("shrink-0", changeStateTone(change.state))}>
            {changeStateLabel(change.state, t)}
          </Badge>
        </div>
        <FeeTermsDiff from={current} to={change} />
        <p className="text-xs text-ink-soft">
          {awaiting
            ? t("admin.fees.pending.awaitingProvisional", "Version {version} awaits the owners' approval. Provisional start: {at}. While anyone holds units, it binds no earlier than 24 hours after they carry it, so the date can move later.", { version: change.version, at: formatMoment(change.effective_from, locale) })
            : kind === "immediate"
              ? t("admin.fees.pending.immediate", "Version {version} takes effect now and is applied within a minute. Nobody holds units, so no notice was sent.", { version: change.version })
              : t("admin.fees.pending.scheduled", "Version {version} takes effect on {at}. Holders have been notified.", { version: change.version, at: formatMoment(change.effective_from, locale) })}
        </p>
        {change.reason.trim() && (
          <blockquote className="border-l-2 border-accent-warn/60 pl-3 text-sm leading-relaxed">{change.reason.trim()}</blockquote>
        )}
        <p className="text-xs text-ink-soft">{t("admin.fees.pending.requested", "Requested {when}", { when: ago(change.requested_at, t) })}</p>
        {owners && <ApprovalStatus change={change} />}
        <NoticeWaiver change={change} />

        {problem && <p className="text-xs text-accent-error">{problem}</p>}

        {confirming ? (
          <Alert variant="destructive" role="status">
            <AlertDescription className="gap-3">
              <p className="text-sm leading-relaxed">{(awaiting ? t("admin.fees.cancelWarningWithdraws", "Cancelling withdraws the change and its proposal from the owners' approvals. The votes given so far stop counting, and a new request starts a new vote.") : t("admin.fees.cancelWarning", "Cancelling withdraws the change. The fund keeps its current terms, and the holders who were notified will not see the new ones."))}</p>
              <div className="flex flex-col gap-2.5 sm:flex-row">
                <Button variant="destructive" size="sm" disabled={busy} onClick={() => void cancel()}>
                  {busy && <Spinner aria-hidden />}
                  {t("admin.fees.cancelConfirm", "Cancel the change")}
                </Button>
                {/* Not "Cancel": beside "Cancel the change" that word answers both ways. */}
                <Button ref={keepRef} variant="ghost" size="sm" disabled={busy} onClick={() => setConfirming(false)}>
                  {t("ui.keep", "Keep it")}
                </Button>
              </div>
            </AlertDescription>
          </Alert>
        ) : (
          <div className="flex flex-wrap gap-2">
            <Button variant="ghost" size="sm" onClick={() => setConfirming(true)}>
              {awaiting && owners ? t("admin.fees.cancelWithdraw", "Cancel and withdraw the proposal") : t("admin.fees.cancelChange", "Cancel this change")}
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
