"use client";

// The way a mark leaves the form: Post records it now, inside the NAV-move guard. A move
// past the guard is refused — there is no per-request override, because one admin with a
// flag could lift the guard and drain pooled cash (banking#232), and putting it to the
// owners is concierge's approvals, which invest is not a client of yet.

import { useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Spinner } from "@evinvest/uikit";

import { postValuation } from "@/entities/admin/api/admin-client";
import type { FundNav } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { PostedMark } from "@/views/admin/valuation/ui/posted-mark";

const TEAL_CTA = "bg-primary text-on-primary hover:bg-primary/90";

export function ValuationActions({
  service,
  aum,
  disabled,
  onPosted,
  onError,
}: {
  service: string;
  aum: string;
  disabled: boolean;
  /** The POST answers with the new mark; the caller publishes it and clears the form. */
  onPosted: (nav: FundNav) => Promise<void> | void;
  onError: (message: string | null) => void;
}) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  // The mark as the hub answered it. Kept whole (it names its own `service`) so switching
  // funds hides it by comparison instead of an effect that races the select.
  const [posted, setPosted] = useState<FundNav | null>(null);

  const post = async () => {
    setBusy(true);
    onError(null);
    try {
      const mark = await postValuation({ service, aum });
      setPosted(mark);
      await onPosted(mark);
    } catch (e) {
      onError(errorMessage(e, t));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      {posted?.service === service && <PostedMark mark={posted} onClose={() => setPosted(null)} />}

      <div className="flex flex-wrap items-center justify-end gap-3">
        <p className="min-w-48 flex-1 text-xs text-ink-soft">{t("admin.valuation.guardHint", "A move past the NAV guard is refused; the owners cannot be asked to lift it from here yet.")}</p>
        <Button type="button" className={TEAL_CTA} disabled={disabled || busy} onClick={() => void post()}>
          {busy ? <Spinner aria-hidden /> : null}
          {t("admin.valuation.postValuation", "Post valuation")}
        </Button>
      </div>
    </div>
  );
}
