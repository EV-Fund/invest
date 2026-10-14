"use client";

// The balance read the invest screens show beside a subscription or an order — what is
// available to deal with. Read-only on purpose: every money action is banking's, run on
// evinvest.ltd through the wallet element (`views/wallet`). Views import from here, not
// from `../api/wallet-client`.

import { fetchWallet } from "@/entities/wallet/api/wallet-client";
import { TAG } from "@/shared/lib/cache-tags";
import { defineResource } from "@/shared/lib/resource";

export const walletResource = defineResource({
  name: "wallet",
  fetch: fetchWallet,
  tags: [TAG.wallet],
});
