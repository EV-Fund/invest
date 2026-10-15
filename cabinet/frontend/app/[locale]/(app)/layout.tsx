import { cookies } from "next/headers";
import { type ReactNode, Suspense } from "react";

import { AppFrame } from "@/application/layout/app-frame";
import { CacheWarmer } from "@/application/layout/cache-warmer";
import { RouteGate } from "@/application/layout/route-gate";
import { SessionAnalytics } from "@/application/layout/session-analytics";
import { SessionKeeper } from "@/application/layout/session-keeper";
import { TimeZoneSync } from "@/application/layout/time-zone-sync";
import { SignInDialog } from "@/features/auth";
import { COOKIES, isSessionId } from "@/shared/config/cookies";
import { renderTimeZone } from "@/shared/config/time-zone";
import { TimeZoneProvider } from "@/shared/lib/time-zone";

// The app frame for every cabinet page. Every page renders for a guest too: the route gate
// walls off what the session does not cover, and the sign-in dialog opens over the dimmed
// frame rather than on a page of its own. SessionKeeper mounts once here to keep the RP
// session's tokens fresh for as long as the frame is open.
//
// Screens show times in the reader's own zone; the provider tells them which zone the
// server rendered in, so hydration reproduces it (`shared/lib/time-zone.tsx`).
export default async function AppLayout({ children }: { children: ReactNode }) {
  const timeZone = await renderTimeZone();
  const signedIn = isSessionId((await cookies()).get(COOKIES.session)?.value);
  return (
    <>
      <SessionKeeper />
      {/* Identifies the PostHog person by user id and records `session_created`, once
          per tab; sits under the same providers as everything else in the shell. */}
      <SessionAnalytics />
      {signedIn && <CacheWarmer />}
      <TimeZoneSync />
      {/* The frame reads the URL (the rail's marker, the sign-in flag), so it renders under
          a boundary of its own rather than holding the whole page to the client. */}
      <Suspense>
        <AppFrame>
          <TimeZoneProvider value={timeZone}>
            <RouteGate signedIn={signedIn}>{children}</RouteGate>
          </TimeZoneProvider>
        </AppFrame>
        <SignInDialog />
      </Suspense>
    </>
  );
}
