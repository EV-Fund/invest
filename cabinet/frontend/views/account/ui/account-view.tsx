"use client";

import { useT } from "@evinvest/i18n/react";

import { platformSignOutHref } from "@evinvest/concierge";
import { AccountPage } from "@evinvest/concierge/react";

import { Skeleton } from "@evinvest/uikit";

import { BASE_PATH, apiPath } from "@/shared/config/base-path";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { useConciergeElement } from "@/shared/lib/platform-mfe";
import { clearResources } from "@/shared/lib/resource";
import { useMfeNavigate } from "@/shared/lib/use-mfe-navigate";
import { PageFrame } from "@/shared/ui/page-frame";
import { ResourceError } from "@/shared/ui/resource-error";

/**
 * Ends the invest RP session, then sends the browser to concierge's confirm page, which
 * ends the evinvest.ltd session too — without that the silent RP login would sign the next
 * person at this browser straight back in.
 *
 * The navigation is ours, not the element wrapper's (`platformSignOut={false}`): the wrapper
 * returns to the host's `/`, which is the landing on this host, while concierge takes a
 * `return_to` only when its path is byte for byte one of the `invest` scope's
 * `return_paths` — `/cabinet`.
 *
 * The element itself navigates to the confirm page right after its `signout` event, so the
 * page may unload before this POST returns; `keepalive` lets the request finish anyway.
 */
async function signOut(): Promise<void> {
  clearResources();
  await fetch(apiPath("/api/auth/logout"), { method: "POST", headers: csrfHeader(), keepalive: true }).catch(() => undefined);
  window.location.assign(platformSignOutHref(`${window.location.origin}${BASE_PATH}`));
}

/** Concierge's account page — profile, security, notifications — as this host's `/account`. */
export function AccountView() {
  const t = useT();
  const element = useConciergeElement("account");
  const onNavigate = useMfeNavigate();

  return (
    <PageFrame title={t("account.title", "Account")} width="content">
      {element.status === "loading" && <Skeleton className="h-96 w-full" />}
      {element.status === "failed" && <ResourceError message={t("account.unavailable", "Your account settings could not be loaded. Reload the page to try again.")} />}
      {element.status === "ready" && (
        <AccountPage scriptUrl={element.entry.scriptUrl} onNavigate={onNavigate} onSignOut={signOut} platformSignOut={false} fallback={<Skeleton className="h-96 w-full" />} />
      )}
    </PageFrame>
  );
}
