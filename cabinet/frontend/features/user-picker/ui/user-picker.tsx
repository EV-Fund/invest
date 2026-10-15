"use client";

// The person a console form names — grants, mints and retirements — by their concierge
// user id. invest keeps no directory and its BFF cannot look another person up (a relying
// party's token opens concierge's `GetMe` and nothing else), so there is nothing to search:
// the operator pastes the id from the central admin, and invest-core refuses one it does not
// know.

import { useT } from "@evinvest/i18n/react";

import { Input } from "@evinvest/uikit";

export interface PickedUser {
  userId: string;
  /** Always `""`: kept so a form can show an email the day the BFF can name people. */
  email: string;
}

/** `labelledBy` is the id of the `FieldLabel` above the field. */
export function UserPicker({ value, onPick, labelledBy }: { value: PickedUser | null; onPick: (user: PickedUser | null) => void; labelledBy?: string }) {
  const t = useT();
  return (
    <Input
      value={value?.userId ?? ""}
      onChange={(e) => {
        const userId = e.target.value.trim();
        onPick(userId ? { userId, email: "" } : null);
      }}
      placeholder={t("admin.alloc.userIdPlaceholder", "Concierge user id")}
      aria-labelledby={labelledBy}
      spellCheck={false}
      autoComplete="off"
      className="w-full font-mono-tech text-sm"
    />
  );
}
