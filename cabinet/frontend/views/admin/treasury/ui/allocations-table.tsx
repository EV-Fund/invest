"use client";

// Who the cash on allocations belongs to, through units (#36): every allocation invest
// knows — the products and the hidden `fee` / `fund` — with its cash, supply, price and
// largest holders. Read, never derived: each figure is its own ledger read, so the table
// sums nothing. banking's own treasury is banking's figure and stays in the central admin.

import { Layers } from "lucide-react";

import { useT } from "@evinvest/i18n/react";
import { Card, CardContent, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Skeleton, Table, TableBody, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import type { AllocationTreasury } from "@/shared/contracts/admin";
import { cn } from "@/shared/lib/cn";
import { Settled } from "@/shared/ui/motion";
import { EDGE_CELL, TABLE_HEAD } from "@/views/admin/lib/table";
import { AllocationLine } from "@/views/admin/treasury/ui/allocation-line";

export function TreasuryAllocations({ allocations }: { allocations: AllocationTreasury[] | null }) {
  const t = useT();
  return (
    <Card>
      <CardContent className="p-0">
        <Settled
          loading={!allocations}
          skeleton={
            <div className="p-6">
              <Skeleton className="h-32 w-full" />
            </div>
          }
        >
          {!allocations ? null : allocations.length === 0 ? (
            <div className="p-8">
              <Empty className="border md:p-6">
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <Layers />
                  </EmptyMedia>
                  <EmptyTitle>{t("admin.treasury.allocations.empty", "No allocations yet")}</EmptyTitle>
                  <EmptyDescription>{t("admin.treasury.allocations.emptyHint", "Every allocation appears here with its cash, units and holders — the products, and the hidden fee and fund allocations once they exist.")}</EmptyDescription>
                </EmptyHeader>
              </Empty>
            </div>
          ) : (
            // Six columns do not fit a phone: the kit's wrapper scrolls the table sideways.
            <Table>
              <TableHeader>
                {/* i18n-max: 14 per header — a long header widens the scroll, not a cell. */}
                <TableRow>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>{t("admin.treasury.allocations.col.allocation", "Allocation")}</TableHead>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>{t("admin.col.state", "State")}</TableHead>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "text-right")}>{t("admin.treasury.allocations.col.cash", "Cash, USDT")}</TableHead>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "text-right")}>{t("admin.alloc.holders.outstanding", "Outstanding")}</TableHead>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "text-right")}>NAV</TableHead>
                  <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>{t("admin.alloc.holders.title", "Holders")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {allocations.map((a) => (
                  <AllocationLine key={a.service} allocation={a} />
                ))}
              </TableBody>
            </Table>
          )}
        </Settled>
      </CardContent>
    </Card>
  );
}
