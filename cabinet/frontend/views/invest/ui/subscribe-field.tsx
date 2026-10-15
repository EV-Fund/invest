"use client";

// The subscribe form's one field: the amount, the action beside it, and the line under it
// — what the amount buys, or why it cannot go through.
//
// One uikit `Field` rather than a `<label>` around the lot: a label wrapping the tip, the
// Max and the input binds to the FIRST labelable element, which is the tip button, and the
// input is left with no accessible name. `htmlFor`/`id` pins the name to the input.

import { useLocale, useT } from "@evinvest/i18n/react";
import { type ReactNode, useId } from "react";

import { Field, FieldDescription, FieldError, FieldLabel, Input } from "@evinvest/uikit";

import type { FundNav } from "@/shared/contracts";
import { TipAnchor } from "@/shared/tips";
import { formatUnits, formatUsdt, fromBaseUnits } from "@/views/invest/lib/format";
import type { SubscribeCheck } from "@/views/invest/lib/subscribe-check";

export function SubscribeField({
  amount,
  check,
  nav,
  action,
  onChange,
}: {
  amount: string;
  check: SubscribeCheck;
  nav: FundNav | null;
  /** The submit, rendered beside the input so the two stay on one row. */
  action: ReactNode;
  onChange: (amount: string) => void;
}) {
  const t = useT();
  const id = useId();
  const hintId = `${id}-hint`;
  const invalid = check.issue !== null;

  return (
    <Field className="gap-2" data-invalid={invalid || undefined}>
      <span className="flex items-center gap-1.5 text-sm">
        <FieldLabel htmlFor={id}>{t("invest.amountUsdt", "Amount (USDT)")}</FieldLabel>
        <TipAnchor anchor="invest.subscribe.amount" />
      </span>
      <span className="flex flex-wrap items-center gap-3">
        <Input
          id={id}
          value={amount}
          onChange={(e) => onChange(e.target.value)}
          inputMode="decimal"
          placeholder="0.00"
          aria-invalid={invalid || undefined}
          aria-describedby={hintId}
          className="min-w-48 flex-1 tabular-nums"
        />
        {action}
      </span>
      <SubscribeHint id={hintId} check={check} nav={nav} />
    </Field>
  );
}

/** One reason at a time, in the order `checkSubscribe` ranks them — the reader gets the
 *  sentence they can act on, not three at once. */
function SubscribeHint({ id, check, nav }: { id: string; check: SubscribeCheck; nav: FundNav | null }) {
  const t = useT();
  const locale = useLocale();
  const { preview, headroom, issue } = check;
  const unitArgs = (units: bigint) => ({ n: Number(fromBaseUnits(units)), units: formatUnits(fromBaseUnits(units), locale) });

  if (issue === "dust") return <FieldError id={id} className="text-xs">{t("invest.dustHint", "Too small to buy a whole unit at {nav} — increase the amount.", { nav: formatUsdt(nav?.nav, locale) })}</FieldError>;
  if (issue === "overCap") return <FieldError id={id} className="text-xs">{t("invest.overCapHint", "Only {n, plural, one {{units} unit is} other {{units} units are}} left before this fund reaches its cap — reduce the amount.", unitArgs(headroom ?? 0n))}</FieldError>;
  return (
    <FieldDescription id={id} className="text-xs tabular-nums">
      {preview !== null ? t("invest.buysUnits", "Buys {n, plural, one {{units} unit} other {{units} units}} at {nav} USDT per unit.", { ...unitArgs(preview), nav: formatUsdt(nav?.nav, locale) }) : t("invest.subscribeIdleHint", "Units are priced at the current NAV; the unit count is fixed at purchase and does not grow.")}
    </FieldDescription>
  );
}
