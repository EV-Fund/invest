// invest-core's refusals of an owner-level fee change, in the reader's language. The BFF
// relays the hub's English sentence verbatim (`shared/lib/hub-refusal.ts`), so each one is
// matched on the fragment that names its condition, after the `DomainError` prefix. A
// sentence that matches nothing returns `null`, and the caller shows the hub's words.

import type { Translate } from "@evinvest/i18n";

import { stripTransportPrefix } from "@/shared/lib/hub-refusal";

type Refusal = "ownersNeedNoHolders" | "ownersNoWaiver" | "ownersRequesterOnly";

// Lowercase fragments of `core/src/application/fees.rs`; a reworded hub sentence falls
// back to the hub's own words, never to a wrong translation.
const FRAGMENTS: readonly (readonly [string, Refusal])[] = [
  ["possible only on a fund with no holders", "ownersNeedNoHolders"],
  ["cannot be waived; withdraw the change instead", "ownersNoWaiver"],
  ["may be withdrawn only by the one who requested it", "ownersRequesterOnly"],
];

function refusalOf(error: unknown): Refusal | null {
  if (!(error instanceof Error)) return null;
  const text = stripTransportPrefix(error.message).toLowerCase();
  return FRAGMENTS.find(([fragment]) => text.includes(fragment))?.[1] ?? null;
}

/** The plain-language sentence for a known refusal, or `null`. */
export function feeRefusal(error: unknown, t: Translate): string | null {
  switch (refusalOf(error)) {
    case "ownersNeedNoHolders":
      return t("admin.fees.err.ownersNeedNoHolders", "These terms need the owners' approval, and someone holds units in this fund. For now, the owners can decide a change only for a fund nobody holds, because invest cannot deliver notices to holders yet.");
    case "ownersNoWaiver":
      return t("admin.fees.err.ownersNoWaiver", "The owners decided this change, so its holders' notices cannot be waived. Withdraw the change instead.");
    case "ownersRequesterOnly":
      return t("admin.fees.err.ownersRequesterOnly", "Only the person who requested this change can withdraw it, because the owners decide it.");
    case null:
      return null;
  }
}
