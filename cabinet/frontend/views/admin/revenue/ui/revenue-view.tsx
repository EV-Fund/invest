"use client";

// Admin console — the platform's OWN money: the reserved `fee` allocation (#245).
//
// An allocation like any other: a cash claim credited by the book's taker fee and settled
// 2-and-20, a supply of units, a NAV, and holders — the people the owners have seated on
// it. The screen's whole job is to make one distinction unmistakable: this is never client
// money and never the `fund` allocation, which are separate claims this surface cannot
// reach. banking's own treasury (D17) is banking's figure and lives in the central admin.
//
// Nothing is written from here. A holder redeems; paying revenue out and seating a holder
// are the owners' approvals in the central admin.

import { useT } from "@evinvest/i18n/react";
import { Skeleton } from "@evinvest/uikit";

import { fundRevenueResource } from "@/entities/admin/model/admin-resource";
import { useResource } from "@/shared/lib/resource";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { FeeAllocationCards } from "@/views/admin/revenue/ui/fee-allocation-cards";
import { WhereNext } from "@/views/admin/revenue/ui/where-next";
import { HoldersTable } from "@/views/admin/ui/holders-table";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

export function RevenueView() {
  const t = useT();
  const revenue = useResource(fundRevenueResource);
  const fee = revenue.data ?? null;
  const failed = !fee && Boolean(revenue.error);

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.revenue", "Revenue stats")} subtitle={t("admin.revenue.subtitle", "What the platform earned — the fee allocation: its cash, its units and who holds them")} />

      {failed && <ResourceError error={revenue.error} onRetry={() => void revenue.refresh()} retrying={revenue.isValidating} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.earned", "The fee allocation")}</p>
        <FeeAllocationCards fee={fee} loading={!fee && !failed} unavailable={failed} />
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.revenue.ownMoneyNote", "This is the platform's own money. Client balances and the fund allocation are separate ledger claims and are not included here — and cannot be reached from this screen.")}</p>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.alloc.holders.title", "Holders")}</p>
        {failed ? (
          // A read that failed is not an empty cap table: the section says so, with the retry.
          <ResourceError error={revenue.error} onRetry={() => void revenue.refresh()} retrying={revenue.isValidating} />
        ) : (
          <Settled loading={!fee} skeleton={<Skeleton className="h-24 w-full" />}>
            {fee && <HoldersTable holders={fee.holders} outstanding={fee.units_outstanding} />}
          </Settled>
        )}
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.whereNext", "Move or inspect it")}</p>
        <WhereNext />
      </StaggerItem>
    </AdminScreen>
  );
}
