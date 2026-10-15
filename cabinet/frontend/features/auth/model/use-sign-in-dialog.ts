"use client";

import { usePathname, useRouter, useSearchParams } from "next/navigation";

import { SIGN_IN_PARAM } from "@/shared/lib/session-gate";

/**
 * The sign-in dialog's state lives in the URL: `?login` opens it over whatever page
 * carries it, so a link from anywhere (the landing, an email, `/login`) opens the same
 * one, and closing it leaves the page as it was.
 */
export function useSignInDialog() {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();

  /** The page, as it reads without the dialog. */
  function without(): string {
    const rest = new URLSearchParams(params);
    rest.delete(SIGN_IN_PARAM);
    const search = rest.toString();
    return search ? `${pathname}?${search}` : pathname;
  }

  return {
    open: params.has(SIGN_IN_PARAM),
    show() {
      const query = new URLSearchParams(params);
      query.set(SIGN_IN_PARAM, "");
      router.replace(`${pathname}?${query.toString()}`, { scroll: false });
    },
    hide() {
      router.replace(without(), { scroll: false });
    },
  };
}
