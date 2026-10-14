import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { WalletView } from "@/views/wallet/ui/wallet-view";

export default async function WalletPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/(app)/wallet/page.tsx")}>
      <WalletView />
    </I18nScope>
  );
}
