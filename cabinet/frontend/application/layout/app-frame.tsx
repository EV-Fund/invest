"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { Boxes, Home, LifeBuoy, LineChart, Percent, PiggyBank, Receipt, UserRound, Wallet, Shield } from "lucide-react";
import type { ReactNode } from "react";

import { AppShell, BottomTabBar, Button, type NavGroup, type NavItem, ShellNav } from "@evinvest/uikit";

import { prefetchRoute } from "@/application/prefetch";
import { SystemBanner } from "@/application/layout/system-banner";
import { allocationsResource } from "@/entities/fund/model/fund-resource";
import { useSignInDialog } from "@/features/auth";
import { SUPPORT_EMAIL } from "@/shared/config/support";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { useResource } from "@/shared/lib/resource";
import { visibleFor } from "@/shared/lib/roles";
import { useSession } from "@/shared/lib/use-session";
import { Link } from "@/shared/ui/cabinet-link";
import { productGlyph } from "@/shared/ui/icons/products";
import { Logo } from "@/shared/ui/logo";

// Zone-relative hrefs: `Link` puts the locale on and the router the `/cabinet` prefix.
const portfolio = (t: Translate): NavItem => ({ id: "portfolio", href: "/", label: t("nav.portfolio", "Portfolio"), icon: Home });
// Exactly `/invest`: each product has its own row, and a prefix match would light both.
const invest = (t: Translate): NavItem => ({ id: "invest", href: "/invest", label: t("nav.invest", "Invest"), icon: LineChart, match: "exact" });
const wallet = (t: Translate): NavItem => ({ id: "wallet", href: "/wallet", label: t("nav.wallet", "Wallet"), icon: Wallet });
const account = (t: Translate): NavItem => ({ id: "account", href: "/account", label: t("nav.account", "Account"), icon: UserRound });

// The allocation console (D11). Fees is narrower than the group: the BFF admits only the
// admin and owner roles to `/api/admin/fees/*`, so an operator would land on a screen of
// 403s (banking#269).
const ADMIN: readonly { item: (t: Translate) => NavItem; roles?: readonly string[] }[] = [
  { item: (t) => ({ id: "admin-allocations", href: "/admin/allocations", label: t("nav.allocations", "Allocations"), icon: Boxes }) },
  { item: (t) => ({ id: "admin-valuation", href: "/admin/valuation", label: t("nav.valuation", "Valuation & redemptions"), icon: Receipt }) },
  { item: (t) => ({ id: "admin-fees", href: "/admin/fees", label: t("nav.fees", "Fees"), icon: Percent }), roles: ["admin", "owner"] },
  { item: (t) => ({ id: "admin-revenue", href: "/admin/revenue", label: t("nav.revenue", "Revenue stats"), icon: PiggyBank }) },
];

/**
 * The signed-in frame: the kit's rail on a wide screen, its tab bar on a phone. Visibility
 * is decided here — the kit draws only what it is handed.
 */
export function AppFrame({ children }: { children: ReactNode }) {
  const t = useT();
  const pathname = useCabinetPathname();
  const session = useSession();
  const dialog = useSignInDialog();
  const guest = session !== null && !session.authenticated;
  const isAdmin = session?.user?.isAdmin ?? false;
  const role = session?.user?.role;
  // The open allocation registry, not a fixed list: a fund appears in the rail because an
  // operator registered and opened it. A failed read leaves the group out rather than
  // blocking the rail — the nav is not the place to surface an API error.
  const products = useResource(allocationsResource).data?.allocations ?? [];

  const admin = ADMIN.filter((entry) => visibleFor(entry.roles, role)).map((entry) => entry.item(t));
  const groups: NavGroup[] = [
    { id: "account", label: t("nav.group.fund", "Your account"), items: [portfolio(t), invest(t), wallet(t)] },
    ...(products.length > 0
      ? [
          {
            id: "products",
            label: t("invest.products", "Products"),
            // A product's row owns its page and the terminal under it.
            items: products.map((p): NavItem => ({ id: `product-${p.service}`, href: `/invest/${encodeURIComponent(p.service)}`, label: p.title, icon: productGlyph(p.icon) })),
          },
        ]
      : []),
    ...(isAdmin ? [{ id: "admin", label: t("admin.eyebrow.administer", "Administer"), items: admin }] : []),
  ];
  const footerGroups: NavGroup[] = [
    {
      id: "self",
      items: [
        account(t),
        // A mail link: there is no support page to be "on", so it never takes the marker.
        { id: "support", href: `mailto:${encodeURIComponent(SUPPORT_EMAIL)}`, label: t("nav.support", "Support"), icon: LifeBuoy, external: true },
      ],
    },
  ];
  // Five is the most a phone holds. The console collapses to one tab standing in for its rows.
  const tabs: NavItem[] = [
    portfolio(t),
    { ...invest(t), match: "prefix" },
    wallet(t),
    ...(isAdmin ? [{ id: "admin", href: "/admin/allocations", label: t("nav.admin", "Admin"), icon: Shield, also: ["/admin"] }] : []),
    account(t),
  ];
  const warm = (item: NavItem) => {
    if (!item.external) prefetchRoute(item.href);
  };

  return (
    <AppShell
      rail={
        <ShellNav
          className="w-62"
          groups={groups}
          footerGroups={footerGroups}
          pathname={pathname}
          linkComponent={Link}
          onItemIntent={warm}
          header={<Logo className="h-6" />}
          footer={
            guest ? (
              // A guest's way in, wherever they are: the same dialog every wall opens.
              <Button type="button" variant="outline" className="w-full" onClick={dialog.show}>
                {t("auth.wall.action", "Sign in")}
              </Button>
            ) : undefined
          }
          labels={{ primary: t("nav.a11y.primary", "Primary"), footer: t("nav.a11y.secondary", "Secondary") }}
        />
      }
      tabBar={<BottomTabBar items={tabs} pathname={pathname} linkComponent={Link} onItemIntent={warm} labels={{ nav: t("nav.a11y.primary", "Primary") }} />}
      banner={<SystemBanner />}
    >
      {children}
    </AppShell>
  );
}
