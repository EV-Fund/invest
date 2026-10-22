"use client";

// One allocation's line: what it is, where its cash lives and how much, its supply, its
// price and who holds it. Read-only — every figure is changed on its own screen.

import { useLocale, useT } from "@evinvest/i18n/react";
import { TableCell, TableRow } from "@evinvest/uikit";

import type { AllocationTreasury } from "@/shared/contracts/admin";
import { cn } from "@/shared/lib/cn";
import { formatUnits, formatUsdt } from "@/shared/lib/money";
import { accessLabel, accessTone } from "@/views/admin/lib/access";
import { EDGE_CELL } from "@/views/admin/lib/table";
import { HoldersCell } from "@/views/admin/treasury/ui/holders-cell";
import { NavCell } from "@/views/admin/treasury/ui/nav-cell";
import { AllocationStateCell } from "@/views/admin/ui/allocation-state-cell";

export function AllocationLine({ allocation: a }: { allocation: AllocationTreasury }) {
  const t = useT();
  const locale = useLocale();
  return (
    <TableRow>
      <TableCell className={EDGE_CELL}>
        <p className="font-medium">{a.title || a.service}</p>
        <p className="font-mono-tech text-xs text-ink-soft">{a.service}</p>
      </TableCell>
      <TableCell className={EDGE_CELL}>
        <div className="flex flex-wrap items-center gap-1.5">
          <AllocationStateCell state={a.state} />
          <span className={cn("inline-flex whitespace-nowrap rounded-full border px-2 py-0.5 text-xs font-medium", accessTone(a.access))}>{accessLabel(a.access, t)}</span>
        </div>
      </TableCell>
      {/* One figure, not posted / available / reserved: the cash is one banking balance and
          invest holds no approvals over it, so the three would always read the same. */}
      <TableCell className={cn(EDGE_CELL, "text-right tabular-nums")}>
        <p>{formatUsdt(a.claim.available, locale)}</p>
        {/* The scope names the banking account the cash sits on — what an operator looks
            up in the central admin to check the figure. */}
        <p className="font-mono-tech text-xs text-ink-soft">{a.scope}</p>
      </TableCell>
      <TableCell className={cn(EDGE_CELL, "text-right tabular-nums")}>{formatUnits(a.units_outstanding, locale)}</TableCell>
      <NavCell allocation={a} />
      <TableCell className={EDGE_CELL}>
        <HoldersCell holders={a.holders} holderCount={a.holder_count} />
      </TableCell>
    </TableRow>
  );
}
