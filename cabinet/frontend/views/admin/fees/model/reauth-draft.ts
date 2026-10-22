// The fee form's draft across the sign-in confirmation (D24(1)). An owner-level change is
// sent in the requester's name only with a sign-in confirmed in the last ~100 s, and the
// confirmation is a full-page round trip through concierge. The draft waits here meanwhile,
// in sessionStorage, so it survives the trip in this tab and nowhere else.
//
// One entry, keyed to its fund inside: one confirmation covers one submit, so a second
// fund's draft replacing the first loses nothing the operator could still send.

import type { TermsDraft } from "@/views/admin/fees/lib/schedule";

const KEY = "invest.fees.reauth-draft";

// Long enough for a slow sign-in, short enough that a tab reopened next day does not open
// on an old draft as if it were just confirmed.
const MAX_AGE_MS = 15 * 60 * 1000;

interface Stored {
  service: string;
  draft: TermsDraft;
  savedAt: number;
}

const DRAFT_FIELDS = ["management", "performance", "hurdle", "basis", "crystallization", "effectiveFrom", "reason"] as const satisfies readonly (keyof TermsDraft)[];

// sessionStorage is outside data: another build of this page may have written it.
function isStored(value: unknown): value is Stored {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  if (typeof v.service !== "string" || typeof v.savedAt !== "number") return false;
  const draft = v.draft;
  if (typeof draft !== "object" || draft === null) return false;
  const d = draft as Record<string, unknown>;
  return DRAFT_FIELDS.every((field) => typeof d[field] === "string");
}

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.sessionStorage;
  } catch {
    // Storage blocked by the browser: the draft is simply not kept.
    return null;
  }
}

function read(): Stored | null {
  const store = storage();
  const raw = store?.getItem(KEY);
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (isStored(parsed) && Date.now() - parsed.savedAt <= MAX_AGE_MS) return parsed;
  } catch {
    // Unparseable: dropped below like an expired one.
  }
  store?.removeItem(KEY);
  return null;
}

export function saveReauthDraft(service: string, draft: TermsDraft): void {
  try {
    storage()?.setItem(KEY, JSON.stringify({ service, draft, savedAt: Date.now() } satisfies Stored));
  } catch {
    // Quota or a blocked store: the operator retypes the terms after the round trip.
  }
}

/** The fund whose draft waits for a submit, so the screen opens on it after the trip. */
export function reauthDraftService(): string | null {
  return read()?.service ?? null;
}

/** The waiting draft for this fund, or `null`. Read, not taken: a second 403 saves it again,
 *  and only a successful submit or a cancel ends it. */
export function reauthDraftFor(service: string): TermsDraft | null {
  const stored = read();
  return stored?.service === service ? stored.draft : null;
}

export function clearReauthDraft(service: string): void {
  if (read()?.service === service) storage()?.removeItem(KEY);
}
