// Browser → BFF balance read. The shape is the proto-derived `Wallet` from
// `@/shared/contracts`; transport and session handling belong to `@/shared/lib/api-client`.
// The invest BFF answers it from banking for the caller's invest compartment.

import { getJson } from "@/shared/lib/api-client";
import type { Wallet } from "@/shared/contracts";

export function fetchWallet(): Promise<Wallet> {
  return getJson<Wallet>("/api/wallet");
}
