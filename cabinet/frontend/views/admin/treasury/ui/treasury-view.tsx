"use client";

// Admin console — held through allocations (#36): every allocation's cash, supply, price
// and largest holders, in one read. It replaces the same table in the central admin and
// the banking cabinet; cash only (owner decision 2026-10-22), so banking's own treasury
// line is not here. Nothing is written from this screen.

import { useT } from "@evinvest/i18n/react";

import { allocationTreasuryResource } from "@/entities/admin/model/admin-resource";
import { useResource } from "@/shared/lib/resource";
import { StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { TreasuryAllocations } from "@/views/admin/treasury/ui/allocations-table";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

export function TreasuryView() {
  const t = useT();
  const treasury = useResource(allocationTreasuryResource);
  const allocations = treasury.data?.allocations ?? null;
  const failed = !allocations && Boolean(treasury.error);

  return (
    <AdminScreen className="space-y-6">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.heldThrough", "Held through allocations")} subtitle={t("admin.treasury.subtitle", "Every allocation's cash, units, price and largest holders — the hidden fee and fund allocations included")} />
      {failed ? (
        // A read that failed is not an empty table: the screen says so, with the retry.
        <ResourceError error={treasury.error} onRetry={() => void treasury.refresh()} retrying={treasury.isValidating} />
      ) : (
        <StaggerItem as="section">
          <TreasuryAllocations allocations={allocations} />
        </StaggerItem>
      )}
    </AdminScreen>
  );
}
