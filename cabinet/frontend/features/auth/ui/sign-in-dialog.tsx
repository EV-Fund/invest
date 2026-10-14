"use client";

import { useT } from "@evinvest/i18n/react";

import { rpLoginHref } from "@evinvest/concierge";
import { SignInDialog as ConciergeSignIn } from "@evinvest/concierge/react";
import { useSearchParams } from "next/navigation";

import { buttonVariants } from "@evinvest/uikit";

import { useSignInDialog } from "@/features/auth/model/use-sign-in-dialog";
import { apiPath } from "@/shared/config/base-path";
import { SIGN_IN_PARAM } from "@/shared/lib/session-gate";
import { useConciergeElement } from "@/shared/lib/platform-mfe";

// The invest BFF's RP login: concierge `authorize` → `code` → its callback, which mints
// the invest session cookie. Silent once the evinvest.ltd session exists.
const RP_LOGIN = apiPath("/api/auth/login");

/** The document path of this page without the dialog's flag — where the RP login lands. */
function currentPage(params: URLSearchParams): string {
  const rest = new URLSearchParams(params);
  rest.delete(SIGN_IN_PARAM);
  const search = rest.toString();
  return `${window.location.pathname}${search ? `?${search}` : ""}`;
}

/**
 * Concierge's sign-in element over the dimmed cabinet. The element draws its own backdrop
 * and traps focus; the registry is read only on the first open.
 */
export function SignInDialog() {
  const t = useT();
  const dialog = useSignInDialog();
  const params = useSearchParams();
  const element = useConciergeElement("signIn", dialog.open);

  // Shown while the bundle loads and for good if it fails: the RP login works without
  // the element, it just takes the full concierge page instead of the dialog.
  const fallback = dialog.open ? (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-background/80">
      <a href={RP_LOGIN} className={buttonVariants()}>
        {t("auth.continue", "Continue to sign in")}
      </a>
    </div>
  ) : null;

  // Ready only after the registry read in an effect, so `window` below is the browser's.
  if (element.status !== "ready") return fallback;
  return (
    <ConciergeSignIn
      scriptUrl={element.entry.scriptUrl}
      open={dialog.open}
      onOpenChange={(open) => !open && dialog.hide()}
      onSignedIn={() => window.location.assign(rpLoginHref(currentPage(params), RP_LOGIN))}
      // An OAuth round trip (Google, GitHub) leaves for evinvest.ltd and comes back here.
      // Concierge cuts the query, so the page itself could not reopen the dialog; the RP
      // login can finish the sign-in on its own and land on the cabinet.
      returnTo={`${window.location.origin}${RP_LOGIN}`}
      fallback={fallback}
    />
  );
}
