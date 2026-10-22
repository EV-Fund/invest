"use client";

// An allocation's price and how old it is. A price nobody has marked, or one older than
// invest-core deals on, is the figure an operator must not take at face value, so both
// say so in words and in the warning tone — the tone alone would not reach a screen reader.

import { useLocale, useT } from "@evinvest/i18n/react";
import { TableCell } from "@evinvest/uikit";

import type { AllocationTreasury } from "@/shared/contracts/admin";
import { cn } from "@/shared/lib/cn";
import { formatNav } from "@/shared/lib/money";
import { hasUnixStamp } from "@/shared/lib/unix-stamp";
import { ago } from "@/views/admin/lib/format";
import { EDGE_CELL } from "@/views/admin/lib/table";

export function NavCell({ allocation: a }: { allocation: Pick<AllocationTreasury, "nav" | "nav_posted_at" | "nav_stale"> }) {
  const t = useT();
  const locale = useLocale();
  // `"0"` is "never marked" — priced at seed — not a mark at the epoch.
  const marked = hasUnixStamp(a.nav_posted_at);
  const note = !marked
    ? { text: t("admin.treasury.allocations.unmarked", "not yet marked"), hint: t("admin.treasury.allocations.unmarkedHint", "No mark posted yet: priced at the seed NAV") }
    : a.nav_stale
      ? { text: t("admin.treasury.allocations.stale", "stale · marked {ago}", { ago: ago(a.nav_posted_at, t) }), hint: t("admin.treasury.allocations.staleHint", "The last mark is more than 24 h old: deals and issuances are refused until a new one") }
      : { text: ago(a.nav_posted_at, t), hint: undefined };
  return (
    <TableCell className={cn(EDGE_CELL, "text-right tabular-nums")}>
      <p>{formatNav(a.nav, locale)}</p>
      <p className={cn("text-xs", note.hint ? "font-medium text-accent-warn" : "text-ink-soft")} title={note.hint}>
        {note.text}
      </p>
    </TableCell>
  );
}
