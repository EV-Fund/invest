// The cabinet's contract with the invest BFF — import from `@/shared/contracts`.
//
// Hand-written against the BFF's DTOs (`cabinet/backend/src/dto.rs`): the BFF serialises
// its own serde structs, publishes no OpenAPI, and every shape here is one of them. A DTO
// change there is mirrored here in the same change. Snake_case; 64-bit values and
// timestamps are strings. `/api/auth/session` alone is camelCase (`SessionInfo`).

export type { AccruedFees, FundNavHistory, NavMark, ParticipationPoint, Position, PositionList, RedemptionList, Subscription, SubscriptionPhase, SubscriptionState } from "./funds";
export { subscriptionPhase } from "./funds";

// The catalog is served to the investor surface (`/api/allocations`, what this caller may
// see) and the console (`/api/admin/allocations`, everything) from the same DTO, so these
// live once in `./admin` and are re-exported here for the investor imports.
export type {
  Allocation,
  AllocationAccessGrant,
  AllocationAccessGrantList,
  AllocationAccessLevel,
  AllocationBacking,
  AllocationClaim,
  AllocationGrantLevel,
  AllocationIcon,
  AllocationList,
  AllocationState,
  AllocationTreasury,
  FeePolicy,
  FundNav,
  Redemption,
  UnitHolderKind,
  UnitHolderRef,
  UnitHolding,
} from "./admin";
