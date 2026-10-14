"use client";

import { useLocale, useT } from "@evinvest/i18n/react";
import { type CSSProperties, useCallback } from "react";

import { Card } from "@evinvest/uikit";

import { allocationsResource, positionsResource } from "@/entities/fund/model/fund-resource";
import { cn } from "@/shared/lib/cn";
import { useHydrated } from "@/shared/lib/use-hydrated";
import { type ResourceSeed, useSeededResource } from "@/shared/lib/resource";
import type { AllocationList, PositionList } from "@/shared/contracts";
import { StaggerItem } from "@/shared/ui/motion";
import { PageFrame } from "@/shared/ui/page-frame";
import { formatCount, STAT_STRIP, StatDivider, StatTile } from "@/shared/ui/stat-tile";
import { CARD_FROM_LG } from "@/views/dashboard/lib/chrome";
import { formatSignedUsd, formatUsd, num, valence } from "@/views/dashboard/lib/format";
import { summariseHoldings, toAllocations } from "@/views/dashboard/lib/holdings";
import { PerfCard } from "@/views/dashboard/ui/perf-card";
import { WhatIOwnCard } from "@/views/dashboard/ui/what-i-own-card";

// The portfolio (Figma `cabinet/home`, without its money cards): what the caller holds
// across allocations and how it has done. Cash, deposits and the activity timeline are the
// wallet's — banking's element on `/wallet` — so nothing here reads the balance.
//
// `initial` is the server's read of the same two (see the route) — absent while it streams,
// and per read: one that failed or timed out is simply missing, and that card makes the
// browser read it always made.
export interface DashboardSeed {
  positions?: ResourceSeed<PositionList>;
  catalog?: ResourceSeed<AllocationList>;
}

export function DashboardView({ initial }: { initial?: DashboardSeed }) {
  const t = useT();
  const locale = useLocale();
  // Bound once per locale, not inline: AnimatedNumber restarts its count whenever the
  // identity of `format` changes, and a fresh closure every render would restart it every
  // render.
  const usd = useCallback((n: number) => formatUsd(n, locale), [locale]);
  const signedUsd = useCallback((n: number) => formatSignedUsd(n, locale), [locale]);
  // Both reads are shared with other screens and cached, so a return paints the holdings on
  // the first frame. The catalog is the same registry the rail lists products from: a fund
  // row should name the product, not the slug that keys it.
  const positions = useSeededResource(positionsResource, initial?.positions);
  const catalogRead = useSeededResource(allocationsResource, initial?.catalog);
  // The arrival is for a screen the browser draws, not for figures already in the HTML.
  const entrance = useHydrated() && !initial?.positions;
  const catalog = catalogRead.data?.allocations ?? [];

  const pos = positions.data?.positions ?? [];
  const { pnl, netContributed, allTimePct } = summariseHoldings(pos);
  const posLoading = positions.isLoading;
  const value = posLoading ? undefined : String(pos.reduce((sum, p) => sum + num(p.value), 0));

  const titleOf = (service: string | undefined) => (service ? (catalog.find((a) => a.service === service)?.title ?? service) : t("dash.fundFallback", "Fund"));
  // The chart is per allocation, never "the fund" (#245): the first one the caller holds,
  // or — for an account that holds nothing yet — the first open product, so a new investor
  // sees what the curve of the thing on offer looks like. `null` until both reads have
  // answered, so the plot shows a skeleton rather than an empty state that then fills.
  const allocation = posLoading || catalogRead.isLoading ? null : (pos[0]?.service ?? catalog[0]?.service ?? "");

  // One DOM order, two layouts: mobile stacks in reading order (hero → figures → what I
  // own); from `xl` the same children sit on a two-column grid. The side track is a fixed
  // 360px with no matching step on the spacing scale, so it rides in as a custom property.
  return (
    <PageFrame
      entrance={entrance}
      title={t("dash.portfolio", "Portfolio")}
      description={t("dash.portfolioSub", "All-time performance and your participation")}
      // Desktop only: on mobile the hero label carries the page.
      headingClassName="hidden lg:flex xl:col-span-2 xl:row-start-1"
      className="grid grid-cols-1 xl:grid-cols-(--dash-columns) xl:items-start"
      style={{ "--dash-columns": "minmax(0, 1fr) 360px" } as CSSProperties}
    >
      <PerfCard value={value} loading={posLoading} allTimePct={allTimePct} allocation={allocation} className="lg:order-1 xl:col-start-1 xl:row-start-2" />
      <WhatIOwnCard allocations={toAllocations(pos, titleOf)} loading={posLoading} className="lg:order-2 xl:col-start-2 xl:row-start-2" />

      {/* stat strip — a card grid on mobile, one divided strip from `lg` */}
      <StaggerItem as={Card} className={cn(STAT_STRIP, CARD_FROM_LG, "lg:order-3 xl:col-span-2 xl:col-start-1 xl:row-start-3")}>
        <StatTile label={t("dash.unrealizedPnl", "Unrealized P&L")} value={posLoading ? null : pnl} format={signedUsd} tone={valence(pnl)} hint={t("dash.hintAcrossPositions", "across all positions")} tip="dashboard.stats.unrealized-pnl" />
        <StatDivider />
        <StatTile label={t("dash.activeStrategies", "Active strategies")} value={posLoading ? null : pos.length} format={formatCount} hint={t("dash.hintFundPositions", "fund positions")} />
        <StatDivider />
        <StatTile label={t("dash.netContributed", "Net contributed")} value={posLoading ? null : netContributed} format={usd} hint={t("dash.hintAtCostBasis", "at cost basis")} tip="dashboard.stats.net-invested" />
      </StaggerItem>
    </PageFrame>
  );
}
