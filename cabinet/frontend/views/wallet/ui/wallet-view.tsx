"use client";

import { useT } from "@evinvest/i18n/react";

import { Wallet } from "@evinvest/wallet/react";

import { Skeleton } from "@evinvest/uikit";

import { TAG } from "@/shared/lib/cache-tags";
import { revalidateTag } from "@/shared/lib/resource";
import { useMfeNavigate } from "@/shared/lib/use-mfe-navigate";
import { useWalletElement } from "@/shared/lib/platform-mfe";
import { PageFrame } from "@/shared/ui/page-frame";
import { ResourceError } from "@/shared/ui/resource-error";

// The invest scope this host runs; the element shows its compartment beside main.
const SCOPE = "invest";

/**
 * Banking's wallet, scoped to invest: balances and history here, every money action in a
 * popup on evinvest.ltd (D8, D9). The cabinet never moves money itself.
 */
export function WalletView() {
  const t = useT();
  const element = useWalletElement("wallet");
  const onNavigate = useMfeNavigate();

  return (
    <PageFrame title={t("wallet.title", "Wallet")} description={t("wallet.subtitle", "Your invest balance and the main account behind it")} width="content">
      {element.status === "loading" && <Skeleton className="h-64 w-full" />}
      {element.status === "failed" && <ResourceError message={t("wallet.unavailable", "The wallet could not be loaded. Reload the page to try again.")} />}
      {element.status === "ready" && (
        <Wallet
          scriptUrl={element.entry.scriptUrl}
          scope={SCOPE}
          onNavigate={onNavigate}
          // A move into or out of the compartment changes what the invest screens show as
          // available. After `closed` or `timeout` the action may still have gone through,
          // so every outcome re-reads.
          onResult={() => revalidateTag(TAG.wallet)}
          fallback={<Skeleton className="h-64 w-full" />}
        />
      )}
    </PageFrame>
  );
}
