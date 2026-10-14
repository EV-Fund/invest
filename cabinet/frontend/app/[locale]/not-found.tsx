import type { Metadata } from "next";

import { currentLocale } from "@/shared/config/locale";
import { LocalisedStatus } from "@/views/status";

// Explicit noindex: the cabinet is behind a session gate, but it shares the public
// invest origin with the landing, so state it rather than rely on Next's built-in
// not-found meta.
export const metadata: Metadata = { robots: { index: false } };

// The 404 for the whole `/cabinet/{locale}` tree — reached by `notFound()` from a
// route that matched. Next hands this page no props at all, which is why the locale
// comes from `currentLocale()` (`next/root-params`) rather than `params`.
export default async function NotFoundPage() {
  return <LocalisedStatus kind="notFound" locale={await currentLocale()} />;
}
