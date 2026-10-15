"use client";

// The subscribe form. Extracted from the invest list when the product page arrived: a
// subscription has to behave identically wherever it is initiated from, and the surest
// way to guarantee that is one implementation.
//
// No `onDone`: `submitSubscribe` names what it moved (see `entities/fund/model/fund-resource`),
// so the page around this — and Home — refreshes itself.
//
// A subscription is a saga across banking's cash and invest's units: the answer is usually
// `reserved` — banking holds the cash, the units are minted when the relay captures it — so
// the receipt says pending, not minted. Banking's refusals (balance, verification) arrive as
// a `failed` subscription with banking's reason, which the receipt quotes.

import { useAnalytics } from "@evinvest/analytics/react";
import { useLocale, useT } from "@evinvest/i18n/react";
import { Hourglass, Sparkles, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { Alert, AlertDescription, AlertTitle, Button, Spinner } from "@evinvest/uikit";

import { hasHoldings } from "@/entities/fund/lib/holdings";
import { positionsResource, submitSubscribe } from "@/entities/fund/model/fund-resource";
import type { FundNav, Subscription } from "@/shared/contracts";
import { subscriptionPhase } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { Link } from "@/shared/ui/cabinet-link";
import { Panel, PanelPresence } from "@/shared/ui/motion-panel";
import { recordFirstSubscription } from "@/views/invest/lib/first-subscription";
import { formatExactUsdt, formatUnits, formatUsdt } from "@/views/invest/lib/format";
import { canSubmit, checkSubscribe } from "@/views/invest/lib/subscribe-check";
import { SubscribeField } from "@/views/invest/ui/subscribe-field";

export function SubscribePanel({ service, nav }: { service: string; nav: FundNav | null }) {
  const t = useT();
  const locale = useLocale();
  const [amount, setAmount] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [done, setDone] = useState<Subscription | null>(null);
  const capture = useAnalytics();

  const check = checkSubscribe({ amount, nav });

  const submit = async () => {
    if (submitting || !canSubmit(check)) return;
    setSubmitting(true);
    setError(null);
    setDone(null);
    // Read BEFORE the submit: the mutation refreshes the positions, and afterwards the
    // holding this subscription just opened would be the "previous" one.
    const held = hasHoldings(positionsResource.peek());
    try {
      const receipt = await submitSubscribe({ service, amount });
      setDone(receipt);
      if (subscriptionPhase(receipt.state) !== "failed") {
        setAmount("");
        recordFirstSubscription(capture, service, held);
      }
    } catch (e) {
      // The error itself: `errorMessage` resolves its `code` in the reader's locale.
      setError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-border bg-secondary p-4">
      {/* The receipt and the failure occupy the same slot and replace one another, so
          they share a presence boundary: retrying after an error swaps the panel in
          place instead of collapsing the form and re-expanding it. */}
      <PanelPresence>
        {done && (
          <Panel key={`receipt-${done.id}`} from="bottom">
            <SubscriptionReceipt subscription={done} />
          </Panel>
        )}
        {!!error && (
          <Panel key="error" from="bottom">
            <Alert variant="destructive">
              <TriangleAlert className="size-4" />
              <AlertTitle>{t("invest.subscribeFailed", "Subscription failed")}</AlertTitle>
              <AlertDescription>{errorMessage(error, t)}</AlertDescription>
            </Alert>
          </Panel>
        )}
      </PanelPresence>

      <SubscribeField
        amount={amount}
        check={check}
        nav={nav}
        onChange={setAmount}
        action={
          <Button type="button" className="tabular-nums" disabled={submitting || !canSubmit(check)} onClick={submit}>
            {submitting ? <Spinner aria-hidden /> : <Sparkles className="size-4" />}
            {/* Names the outcome once there is one to name, and names it EXACTLY: the check
                runs at 18 dp, so a rounding formatter could read "0.00" over an amount
                that still buys a fraction of a unit and submits. */}
            {check.preview !== null ? t("invest.investAmount", "Invest {amount} USDT", { amount: formatExactUsdt(amount, locale) }) : t("invest.subscribe", "Subscribe")}
          </Button>
        }
      />
    </div>
  );
}

/** The subscription as the saga left it: minted, on its way, or refused with banking's reason. */
function SubscriptionReceipt({ subscription }: { subscription: Subscription }) {
  const t = useT();
  const locale = useLocale();
  const args = { n: Number(subscription.units || 0), units: formatUnits(subscription.units, locale), nav: formatUsdt(subscription.nav, locale) };
  const link = "rounded-sm font-medium text-primary-ink underline underline-offset-2 outline-none focus-visible:ring-2 focus-visible:ring-ring";

  switch (subscriptionPhase(subscription.state)) {
    case "settled":
      return (
        <Alert>
          <Sparkles className="size-4 text-positive" />
          <AlertTitle>{t("invest.subscribeReceived", "Subscription received")}</AlertTitle>
          <AlertDescription>{t("invest.subscribeReceiptBody", "Minted {n, plural, one {{units} unit} other {{units} units}} at {nav} USDT NAV — your position updates shortly.", args)}</AlertDescription>
        </Alert>
      );
    case "pending":
      return (
        <Alert>
          <Hourglass className="size-4 text-accent-warn" />
          <AlertTitle>{t("invest.subscribePending", "Subscription pending")}</AlertTitle>
          <AlertDescription>{t("invest.subscribePendingBody", "Your cash is on hold. {n, plural, one {{units} unit} other {{units} units}} at {nav} USDT NAV are minted once it is captured — your position updates then.", args)}</AlertDescription>
        </Alert>
      );
    case "failed":
      return (
        <Alert variant="destructive">
          <TriangleAlert className="size-4" />
          <AlertTitle>{t("invest.subscribeFailed", "Subscription failed")}</AlertTitle>
          <AlertDescription className="space-y-1">
            {/* banking's own words, untranslated: the one account of why it refused. */}
            <p>{subscription.failure_reason || t("invest.subscribeRefused", "The cash for this subscription was not taken, and nothing moved.")}</p>
            <p className="flex flex-wrap gap-x-3">
              <Link href="/wallet" className={link}>
                {t("invest.topUp", "Top up")}
              </Link>
              <Link href="/account" className={link}>
                {t("invest.cta.verify", "Verify to invest")}
              </Link>
            </p>
          </AlertDescription>
        </Alert>
      );
  }
}
