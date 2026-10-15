"use client";

import { useT } from "@evinvest/i18n/react";

import type { ReactNode } from "react";

import { AuthWall, Button } from "@evinvest/uikit";

import { useSignInDialog } from "@/features/auth";
import { meets, requirementOf } from "@/shared/config/route-permissions";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { SESSION_UNAVAILABLE, useSession } from "@/shared/lib/use-session";
import { ForbiddenScreen } from "@/shared/ui/forbidden-screen";

/**
 * What the page shows this caller: itself when the session meets what the section needs
 * (`shared/config/route-permissions.ts`), a sign-in wall for a guest, a 403 for an account
 * that lacks it. Cosmetic in the sense the BFF is the boundary — but it is what keeps a
 * guest's page from firing a request that could only answer 401.
 *
 * `signedIn` is the server's word on whether a session cookie came with the page: until the
 * session is read, a signed-in reader gets the page and a guest the wall.
 */
export function RouteGate({ signedIn, children }: { signedIn: boolean; children: ReactNode }) {
  const t = useT();
  const session = useSession();
  const pathname = useCabinetPathname();
  const dialog = useSignInDialog();

  const requirement = requirementOf(pathname);
  const unknown = session === null || session === SESSION_UNAVAILABLE;
  const guest = unknown ? !signedIn : !session.authenticated;
  const allowed = requirement === null || (unknown ? signedIn : meets(requirement, session));

  if (allowed) return <>{children}</>;
  if (guest) {
    return (
      <AuthWall
        className="m-4 min-h-[60dvh] sm:m-8"
        title={t("auth.wall.title", "Sign in to see this")}
        description={t("auth.wall.desc", "This part of the cabinet belongs to your account. Sign in or create one — it takes a minute.")}
      >
        <Button type="button" onClick={dialog.show}>
          {t("auth.wall.action", "Sign in")}
        </Button>
      </AuthWall>
    );
  }
  return <ForbiddenScreen />;
}
