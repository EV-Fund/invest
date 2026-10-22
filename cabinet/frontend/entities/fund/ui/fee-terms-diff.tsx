"use client";

// A change of fee terms as a reader compares it: five rows, what the fund charges NOW
// beside what is PROPOSED, the rows that move set in the accent so the eye lands on them.
//
// Ported from the banking cabinet's `entities/fund/ui/fee-terms-diff.tsx`, where the owners
// read it on their approval page. Here the operator reads it on the pending change: the
// words and the rendering stay the same, so both sides of a vote compare the same five
// fields. `from` absent means the fund charges nothing — a different fact from a policy of
// zeros, and the column says so rather than printing 0%.

import { useT } from "@evinvest/i18n/react";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { basisLabel, crystallizationLabel, type FeeTermsLike } from "@/shared/lib/fee-terms";
import { pct } from "@/shared/lib/rate";

// Plain, not tracked-uppercase: the table sits inside a card among prose.
const HEAD = "h-8 px-3 text-xs font-medium text-ink-soft";

export function FeeTermsDiff({ from, to }: { from: FeeTermsLike | null | undefined; to: FeeTermsLike }) {
  const t = useT();
  const rows: { key: string; label: string; now: string | null; next: string }[] = [
    { key: "management", label: t("admin.fees.field.management", "Management"), now: from ? t("invest.perAnnum", "{pct} p.a.", { pct: pct(from.management_bps) }) : null, next: t("invest.perAnnum", "{pct} p.a.", { pct: pct(to.management_bps) }) },
    { key: "performance", label: t("admin.fees.field.performance", "Performance"), now: from ? t("invest.ofTheGain", "{pct} of the gain", { pct: pct(from.performance_bps) }) : null, next: t("invest.ofTheGain", "{pct} of the gain", { pct: pct(to.performance_bps) }) },
    { key: "hurdle", label: t("admin.fees.field.hurdle", "Hurdle"), now: from ? pct(from.hurdle_bps) : null, next: pct(to.hurdle_bps) },
    { key: "basis", label: t("admin.fees.chargedOn", "Charged on"), now: from ? basisLabel(from.basis, t) : null, next: basisLabel(to.basis, t) },
    { key: "period", label: t("invest.lockedIn", "Locked in"), now: from ? crystallizationLabel(from.crystallization, t) : null, next: crystallizationLabel(to.crystallization, t) },
  ];
  return (
    // The kit's wrapper already scrolls; the border goes on the outside of that wrapper.
    <div className="rounded-lg border border-border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className={HEAD} />
            <TableHead className={HEAD}>{t("fund.feeDiff.now", "Now")}</TableHead>
            <TableHead className={HEAD}>{t("fund.feeDiff.proposed", "Proposed")}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {/* The kit's cells are nowrap; in a half-width card a French or Russian label has
              to wrap, not push the table into a sideways scroll. */}
          {rows.map((row) => {
            const moved = row.now !== row.next;
            return (
              <TableRow key={row.key}>
                <TableCell className="whitespace-normal px-3 text-ink-soft">{row.label}</TableCell>
                <TableCell className="whitespace-normal px-3 tabular-nums text-ink-soft">{row.now ?? t("fund.feeDiff.nothingCharged", "Nothing charged")}</TableCell>
                <TableCell className={cn("whitespace-normal px-3 tabular-nums", moved ? "font-semibold text-accent-warn" : "text-ink")}>{row.next}</TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
    </div>
  );
}
