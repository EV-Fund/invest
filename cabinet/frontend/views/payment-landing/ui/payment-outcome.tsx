"use client";

import { CheckCircle2, Info, TriangleAlert } from "lucide-react";
import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import { Alert, AlertDescription, AlertTitle, type AlertVariant } from "@evinvest/uikit";
import type { WalletActionOutcome } from "@evinvest/wallet";

type Shown = WalletActionOutcome | { status: "invalid_link" };

/**
 * Whether the button may come back. After `result` the user has answered, and after
 * `closed` or `timeout` they may have: the answer page cannot tell the host, so offering
 * the same action again would read as if nothing happened. `failed` is the page's own
 * report that the answer did not go through.
 */
export function mayRepeat(shown: Shown | null): boolean {
  if (shown === null) return true;
  switch (shown.status) {
    case "result":
      return shown.result.outcome === "failed";
    case "blocked":
    case "busy":
    case "aborted":
      return true;
    case "closed":
    case "timeout":
    case "invalid_url":
    case "invalid_link":
    case "unsupported":
      return false;
  }
}

/** What the user is told for each way the popup can end; `done` is a hint, never proof. */
export function PaymentOutcome({ shown }: { shown: Shown }) {
  const t = useT();
  const { variant, title, body, icon } = describe(shown, t);
  return (
    <Alert variant={variant}>
      {icon}
      <AlertTitle>{title}</AlertTitle>
      <AlertDescription>{body}</AlertDescription>
    </Alert>
  );
}

type T = ReturnType<typeof useT>;
const ICON = "size-4";

function describe(shown: Shown, t: T): { variant: AlertVariant; title: string; body: string; icon: ReactNode } {
  const check = t("payment.landing.checkWallet", "Check the payment in your wallet to be sure.");
  switch (shown.status) {
    case "result":
      if (shown.result.outcome === "done") {
        return { variant: "success", icon: <CheckCircle2 className={ICON} />, title: t("payment.landing.done.title", "Your answer was sent"), body: t("payment.landing.done.body", "The payment page reported that you approved it. This is only a hint, because the payment is settled on evinvest.ltd. {check}", { check }) };
      }
      // The answer page sets `reference` to the payment id on Decline and leaves it null on
      // Cancel, which is also what a payment that was already answered or expired shows.
      if (shown.result.outcome === "cancelled" && shown.result.reference === null) {
        return { variant: "info", icon: <Info className={ICON} />, title: t("payment.landing.noAnswer.title", "No new answer was sent"), body: t("payment.landing.noAnswer.body", "The payment window closed without a new answer. The payment may already be answered or expired. {check}", { check }) };
      }
      if (shown.result.outcome === "cancelled") {
        return { variant: "info", icon: <Info className={ICON} />, title: t("payment.landing.cancelled.title", "You declined the payment"), body: t("payment.landing.cancelled.body", "The payment page reported that you declined it. {check}", { check }) };
      }
      return { variant: "destructive", icon: <TriangleAlert className={ICON} />, title: t("payment.landing.failed.title", "The payment page reported an error"), body: t("payment.landing.failed.body", "Your answer may not have been recorded. {check} Then you can try again.", { check }) };
    case "closed":
      return { variant: "info", icon: <Info className={ICON} />, title: t("payment.landing.closed.title", "The payment window was closed"), body: t("payment.landing.closed.body", "We cannot tell whether you answered before you closed it. {check}", { check }) };
    case "timeout":
      return { variant: "info", icon: <Info className={ICON} />, title: t("payment.landing.timeout.title", "The payment window timed out"), body: t("payment.landing.timeout.body", "We closed it after a long wait, and we cannot tell whether you answered. {check}", { check }) };
    case "busy":
      return { variant: "info", icon: <Info className={ICON} />, title: t("payment.landing.busy.title", "A payment window is already open"), body: t("payment.landing.busy.body", "We brought it to the front. Finish there first.") };
    case "aborted":
      return { variant: "neutral", icon: <Info className={ICON} />, title: t("payment.landing.aborted.title", "The payment window was closed by this page"), body: t("payment.landing.aborted.body", "Open it again when you are ready.") };
    case "blocked":
      return { variant: "destructive", icon: <TriangleAlert className={ICON} />, title: t("payment.landing.blocked.title", "Your browser blocked the payment window"), body: t("payment.landing.blocked.body", "Allow pop-ups for this site, then try again.") };
    case "unsupported":
      return { variant: "destructive", icon: <TriangleAlert className={ICON} />, title: t("payment.landing.unsupported.title", "This browser cannot open the payment window"), body: t("payment.landing.unsupported.body", "Open this link in an up-to-date browser.") };
    case "invalid_url":
    case "invalid_link":
      return { variant: "destructive", icon: <TriangleAlert className={ICON} />, title: t("payment.landing.invalid.title", "This payment link is not valid"), body: t("payment.landing.invalid.body", "Open the link from your notice again, or find the payment in your wallet.") };
  }
}
