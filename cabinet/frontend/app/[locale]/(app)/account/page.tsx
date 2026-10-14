import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { AccountView } from "@/views/account/ui/account-view";

export default async function AccountRoute() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/(app)/account/page.tsx")}>
      <AccountView />
    </I18nScope>
  );
}
