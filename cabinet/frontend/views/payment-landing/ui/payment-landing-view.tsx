"use client";

import { useEffect, useRef, useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button } from "@evinvest/uikit";
import { openWalletAction, walletNavigationHref, walletPaymentUrl, type WalletActionOutcome } from "@evinvest/wallet";

import { PageFrame } from "@/shared/ui/page-frame";
import { Link } from "@/shared/ui/cabinet-link";
import { mayRepeat, PaymentOutcome } from "./payment-outcome";

// The invest scope this host runs; evinvest.ltd takes it to find the origins that may
// receive the result.
const SCOPE = "invest";

/**
 * Landing for a payment-consent notice: names the payment and opens banking's answer page
 * on evinvest.ltd in a popup (D8). The cabinet never reads or answers the payment itself,
 * so what it shows is the popup's outcome, which is a hint and not the payment's state.
 */
export function PaymentLandingView({ paymentId }: { paymentId: string }) {
  const t = useT();
  const url = walletPaymentUrl(paymentId, SCOPE);
  const [outcome, setOutcome] = useState<WalletActionOutcome | null>(null);
  const [waiting, setWaiting] = useState(false);
  const controller = useRef<AbortController | null>(null);

  // Leaving the page closes a popup that would otherwise outlive it and report to nobody.
  useEffect(() => () => controller.current?.abort(), []);

  const review = async () => {
    if (!url) return;
    controller.current = new AbortController();
    setWaiting(true);
    const result = await openWalletAction(url, { signal: controller.current.signal });
    // Unmount aborts the call; there is no page left to show the outcome on.
    if (controller.current.signal.aborted && result.status === "aborted") return;
    setWaiting(false);
    // The scope has no `returnPaths` on the banking side, so no `return_to`: the answer page
    // can only send the user back to its default.
    const href = result.status === "blocked" ? walletNavigationHref(url) : null;
    if (href) window.location.assign(href);
    else setOutcome(result);
  };

  const shown = url ? outcome : ({ status: "invalid_link" } as const);

  return (
    <PageFrame title={t("payment.landing.title", "Payment to review")} description={t("payment.landing.subtitle", "A payment from your account is waiting for your answer.")} width="content">
      {shown && <PaymentOutcome shown={shown} />}
      <div className="flex flex-wrap items-center gap-2.5">
        {url && mayRepeat(shown) && (
          <Button type="button" disabled={waiting} onClick={() => void review()}>
            {waiting ? t("payment.landing.waiting", "Waiting for the payment window…") : t("payment.landing.review", "Review the payment")}
          </Button>
        )}
        <Button asChild variant="outline">
          <Link href="/wallet">{t("payment.landing.openWallet", "Open wallet")}</Link>
        </Button>
      </div>
    </PageFrame>
  );
}
