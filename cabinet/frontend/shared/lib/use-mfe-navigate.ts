"use client";

import { useRouter } from "next/navigation";
import { useCallback } from "react";

import { BASE_PATH } from "@/shared/config/base-path";

/**
 * `onNavigate` for a platform element: a link into this cabinet goes through the router,
 * anything else on the host (the landing) is left to the element's own navigation. The
 * element hands over a validated root-relative path, which carries the `/cabinet` prefix
 * the router adds back itself.
 */
export function useMfeNavigate(): (href: string) => boolean {
  const router = useRouter();
  return useCallback(
    (href: string) => {
      if (href !== BASE_PATH && !href.startsWith(`${BASE_PATH}/`) && !href.startsWith(`${BASE_PATH}?`)) return false;
      router.push(href.slice(BASE_PATH.length) || "/");
      return true;
    },
    [router],
  );
}
