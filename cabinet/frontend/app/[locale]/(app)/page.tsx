import { Suspense } from "react";

import { I18nScope } from "@evinvest/i18n/react";

import { readAllocations, readPositions } from "@/entities/fund/api/fund-server";
import { DashboardView } from "@/views/dashboard/ui/dashboard-view";
import { isClientNavigation } from "@/shared/api/server/navigation";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The cabinet home — the investor's portfolio (Figma `cabinet/home`).
//
// The reads behind its figures are made on the server and the page never waits for them:
// the boundary's fallback is the same view in its loading state, flushed at once, and the
// reads stream in behind it. A read that fails or times out arrives as no seed, and its
// card makes the browser read it always made.
export default async function Page() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/(app)/page.tsx")}>
      <Suspense fallback={<DashboardView />}>
        <SeededDashboard />
      </Suspense>
    </I18nScope>
  );
}

async function SeededDashboard() {
  // A client-side navigation skips the reads: the browser cache answers those (see navigation.ts).
  if (await isClientNavigation()) return <DashboardView />;
  const [positions, catalog] = await Promise.all([readPositions(), readAllocations()]);
  return <DashboardView initial={{ positions: positions ?? undefined, catalog: catalog ?? undefined }} />;
}
