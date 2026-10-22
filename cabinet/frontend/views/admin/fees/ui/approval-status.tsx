"use client";

// Where the owners' decision on a change stands. The owners vote in the central admin, not
// here, so this block only reports and links: the operator follows the vote, and the one
// action they have (cancel) stays on the pending card.

import { useLocale, useT } from "@evinvest/i18n/react";
import { Badge, Button } from "@evinvest/uikit";

import { PLATFORM_APPROVALS_URL } from "@/shared/config/platform-admin";
import type { FeePolicyChange } from "@/shared/contracts/admin";
import { cn } from "@/shared/lib/cn";
import { approvalStateLabel, approvalStateLine, approvalStateTone } from "@/views/admin/fees/lib/approval";

export function ApprovalStatus({ change }: { change: FeePolicyChange }) {
  const t = useT();
  const locale = useLocale();
  const line = approvalStateLine(change, t, locale);
  const id = change.consilium_id ?? null;
  return (
    <section aria-label={t("admin.fees.approval.title", "Owners' approval")} className="space-y-2 rounded-lg border border-border p-3">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <p className="text-xs font-medium text-ink-soft">{t("admin.fees.approval.title", "Owners' approval")}</p>
        <Badge variant="outline" className={cn("shrink-0", approvalStateTone(change.approval_state))}>
          {approvalStateLabel(change, t)}
        </Badge>
      </div>
      {line && <p className="text-sm leading-relaxed">{line}</p>}
      {change.approval_holders !== null && (
        <p className="text-xs text-ink-soft">{t("admin.fees.approval.holders", "The proposal tells the owners that {count, plural, one {# holder} other {# holders}} will be notified.", { count: change.approval_holders })}</p>
      )}
      {id && (
        <p className="text-xs text-ink-soft">
          {t("admin.fees.approval.id", "Approval id")} <span className="font-mono-tech break-all text-ink">{id}</span>
        </p>
      )}
      <Button asChild size="sm" variant="outline">
        <a href={PLATFORM_APPROVALS_URL}>{t("admin.fees.openApprovals", "Open the owners' approvals")}</a>
      </Button>
    </section>
  );
}
