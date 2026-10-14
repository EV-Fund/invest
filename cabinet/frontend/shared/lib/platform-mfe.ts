"use client";

// The platform's element bundles this cabinet mounts: the concierge sign-in dialog and
// account page, the banking wallet. Each producer publishes a registry on evinvest.ltd; the
// readers in @evinvest/concierge and @evinvest/wallet refuse a bundle outside the directory
// the CSP allows (`shared/config/security.ts`), so a registry that passes here also loads.

import { useEffect, useState } from "react";

import { type ConciergeElement, conciergeEntry, fetchConciergeRegistry } from "@evinvest/concierge";
import type { MfeEntry, ParseResult } from "@evinvest/mfe";
import { type WalletElement, fetchWalletRegistry, walletEntry } from "@evinvest/wallet";

/** `failed` is final for this page: the caller renders its fallback, not a retry loop. */
export type PlatformElement = { status: "loading" } | { status: "ready"; entry: MfeEntry } | { status: "failed" };

// One registry read per page and producer, shared by every element of it. A failed read
// is dropped so the next mount — a later navigation — asks again.
let conciergeRegistry: Promise<ParseResult<MfeEntry[]>> | null = null;
let walletRegistry: Promise<ParseResult<MfeEntry[]>> | null = null;

function readConcierge(): Promise<ParseResult<MfeEntry[]>> {
  conciergeRegistry ??= fetchConciergeRegistry().then((result) => {
    if (!result.ok) conciergeRegistry = null;
    return result;
  });
  return conciergeRegistry;
}

function readWallet(): Promise<ParseResult<MfeEntry[]>> {
  walletRegistry ??= fetchWalletRegistry().then((result) => {
    if (!result.ok) walletRegistry = null;
    return result;
  });
  return walletRegistry;
}

function settle(registry: ParseResult<MfeEntry[]>, pick: (entries: readonly MfeEntry[]) => ParseResult<MfeEntry>): PlatformElement {
  const entry = registry.ok ? pick(registry.value) : registry;
  return entry.ok ? { status: "ready", entry: entry.value } : { status: "failed" };
}

/**
 * A concierge element's registry entry. `enabled: false` skips the read — the sign-in
 * dialog asks only once it is first opened, so a signed-in visitor never fetches it.
 */
export function useConciergeElement(element: ConciergeElement, enabled = true): PlatformElement {
  const [state, setState] = useState<PlatformElement>({ status: "loading" });
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    void readConcierge().then((registry) => active && setState(settle(registry, (entries) => conciergeEntry(entries, element))));
    return () => {
      active = false;
    };
  }, [element, enabled]);
  return state;
}

/** A banking wallet element's registry entry. */
export function useWalletElement(element: WalletElement): PlatformElement {
  const [state, setState] = useState<PlatformElement>({ status: "loading" });
  useEffect(() => {
    let active = true;
    void readWallet().then((registry) => active && setState(settle(registry, (entries) => walletEntry(entries, element))));
    return () => {
      active = false;
    };
  }, [element]);
  return state;
}
