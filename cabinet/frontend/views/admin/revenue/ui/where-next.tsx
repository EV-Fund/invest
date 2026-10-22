"use client";

// Where the money on this screen is moved from, where it is shown beside every other
// allocation, and who decides who holds it. Moving money and seating a holder are in the
// central admin on evinvest.ltd, since money stays banking's and the owners' approvals are
// concierge's; the held-through table is this console's own page.
//
// Paying the platform's earnings out is one case of a payment order (from `service:fee`);
// the held-through page is where the same allocation appears beside the products and
// `fund`; seating a holder on it is a proposal to the owners. Cards rather than buttons,
// because each is a destination with a sentence's worth of reason, not an action.

import { ArrowLeftRight, ChevronRight, Layers, Vote } from "lucide-react";
import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import { Card, CardContent } from "@evinvest/uikit";

import { PLATFORM_ADMIN_URL, PLATFORM_APPROVALS_URL } from "@/shared/config/platform-admin";
import { Link } from "@/shared/ui/cabinet-link";

export function WhereNext() {
  const t = useT();
  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      <LinkCard href={`${PLATFORM_ADMIN_URL}/payments`} icon={<ArrowLeftRight className="size-5" />} title={t("nav.payments", "Payments")} body={t("admin.revenue.toPayments", "Moving the platform's earnings — into a product, or to an investor — is a payment order the owners authorise. Open one there. Cash reaches the chain only through a holder's redemption and their own withdrawal.")} />
      <LinkCard href="/admin/treasury" icon={<Layers className="size-5" />} title={t("nav.heldThrough", "Held through allocations")} body={t("admin.revenue.toHeldThrough", "The fee allocation beside every other allocation: its cash, units, NAV and holders. The bank's own treasury stays in the central admin.")} />
      <LinkCard href={PLATFORM_APPROVALS_URL} icon={<Vote className="size-5" />} title={t("nav.approvals", "Approvals")} body={t("admin.revenue.toApprovals", "Seating a person on the fee allocation is the owners' decision. Propose it there; the units appear here once their vote carries.")} />
    </div>
  );
}

const isZoneHref = (href: string): href is `/${string}` => href.startsWith("/");

/** `href` is a page of this console (`/…`, through the router) or the central admin
 *  (`https://…`, another origin — a plain anchor, since the router would 404 there). */
function LinkCard({ href, icon, title, body }: { href: `/${string}` | `https://${string}`; icon: ReactNode; title: string; body: string }) {
  // The link is stretched over the whole card, so the hit target and the ring are the card.
  const linkClass = "flex items-start gap-3 outline-none after:absolute after:inset-0 after:rounded-xl";
  const content = (
    <>
      <span className="mt-0.5 shrink-0 text-primary-ink">{icon}</span>
      <span className="min-w-0 flex-1 space-y-1">
        <span className="block text-sm font-semibold text-ink">{title}</span>
        <span className="block text-xs leading-relaxed text-ink-soft">{body}</span>
      </span>
      <ChevronRight className="mt-0.5 size-4 shrink-0 text-ink-soft" />
    </>
  );
  return (
    <Card className="relative transition-colors has-hover:bg-ink/5 has-focus-visible:ring-2 has-focus-visible:ring-ring">
      <CardContent className="py-5">
        {isZoneHref(href) ? (
          <Link href={href} className={linkClass}>
            {content}
          </Link>
        ) : (
          <a href={href} className={linkClass}>
            {content}
          </a>
        )}
      </CardContent>
    </Card>
  );
}
