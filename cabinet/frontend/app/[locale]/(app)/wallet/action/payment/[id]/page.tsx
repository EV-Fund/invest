import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { PaymentLandingView } from "@/views/payment-landing/ui/payment-landing-view";

// The target of a payment-consent notice for this scope (banking#580). Money stays on
// evinvest.ltd: this page only names the payment and opens the answer page there. The id is
// passed on raw, not decoded — `walletPaymentUrl` refuses anything outside `[A-Za-z0-9_-]`,
// so a mangled link lands on the "not valid" state instead of reaching the popup.
export default async function PaymentLandingPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/(app)/wallet/action/payment/[id]/page.tsx")}>
      <PaymentLandingView paymentId={id} />
    </I18nScope>
  );
}
