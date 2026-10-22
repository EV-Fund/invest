import { I18nScope } from "@evinvest/i18n/react";

import { TreasuryView } from "@/views/admin/treasury/ui/treasury-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// Admin console — held through allocations (#36). The route gate hides it from a seat
// without `invest:allocation:manage`; the BFF is the boundary and refuses it with 403.
export default async function AdminTreasuryPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/(app)/admin/treasury/page.tsx")}>
      <TreasuryView />
    </I18nScope>
  );
}
