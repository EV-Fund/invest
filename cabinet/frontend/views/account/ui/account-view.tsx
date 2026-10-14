"use client";

import { useT } from "@evinvest/i18n/react";

import { AccountPage } from "@evinvest/concierge/react";

import { Skeleton } from "@evinvest/uikit";

import { apiPath } from "@/shared/config/base-path";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { useConciergeElement } from "@/shared/lib/platform-mfe";
import { clearResources } from "@/shared/lib/resource";
import { useMfeNavigate } from "@/shared/lib/use-mfe-navigate";
import { PageFrame } from "@/shared/ui/page-frame";
import { ResourceError } from "@/shared/ui/resource-error";

/**
 * Ends the invest RP session. The wrapper awaits it and then navigates to concierge's
 * confirm page, which ends the evinvest.ltd session too — without that the silent RP login
 * would sign the next person at this browser straight back in. It must not navigate.
 */
async function signOut(): Promise<void> {
  clearResources();
  await fetch(apiPath("/api/auth/logout"), { method: "POST", headers: csrfHeader() }).catch(() => undefined);
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
        <AccountPage scriptUrl={element.entry.scriptUrl} onNavigate={onNavigate} onSignOut={signOut} fallback={<Skeleton className="h-96 w-full" />} />
      )}
    </PageFrame>
  );
}
