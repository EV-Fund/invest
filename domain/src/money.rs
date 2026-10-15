//! Shared money kernel — fixed-point value objects, all pure and wasm-safe.
//!
//! Every amount is an integer count of `10^-18` of its unit ("base units"): [`Usdt`]
//! for cash, [`Shares`] for fund units, [`Nav`] for the price of one unit. invest's
//! ledger holds units only; cash is banking's, and reaches this crate as a decimal
//! USDT figure read through the money port.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// Canonical internal scale: 18 decimals. Every [`Usdt`] is an integer count of
/// `10^-18` USDT, so amounts are exact (no floating point ever touches money).
pub const CANONICAL_DECIMALS: u32 = 18;
/// `10^18` — the canonical fixed-point scale shared by [`Usdt`], [`Shares`], [`Nav`] and
/// the book's `Price`.
pub(crate) const SCALE: u128 = 10u128.pow(CANONICAL_DECIMALS);
/// A USDT amount in canonical 18-decimal base units. Newtype over `u128` so it
/// can't be confused with a raw on-chain amount or an account id; arithmetic is
/// **checked** (a money type must never silently wrap). Serializes as a **string**
/// of base units — `serde_json` has no `u128` support, and a string is exact across
/// JSON consumers (no float/`2^53` loss) for the event log.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Usdt(u128);
impl Usdt {
	pub const ZERO: Usdt = Usdt(0);

	/// Wrap a raw count of canonical (18-dp) base units.
	pub const fn from_base_units(units: u128) -> Self {
		Self(units)
	}

	/// The raw canonical base-unit count (what TigerBeetle stores as a transfer
	/// `amount`).
	pub const fn base_units(self) -> u128 {
		self.0
	}

	pub const fn is_zero(self) -> bool {
		self.0 == 0
	}

	/// Checked add — `None` on overflow rather than wrapping.
	pub fn checked_add(self, other: Usdt) -> Option<Usdt> {
		self.0.checked_add(other.0).map(Usdt)
	}

	/// Checked sub — `None` if it would go negative.
	pub fn checked_sub(self, other: Usdt) -> Option<Usdt> {
		self.0.checked_sub(other.0).map(Usdt)
	}

	/// `floor(self · numer / denom)` — the pro-rata helper a *rate* needs: a fee in
	/// basis points over a fraction of a year is one `scale` call, with no intermediate
	/// rounding to lose. The product is 256-bit (see [`mul_div_floor`]), so a whole
	/// fund's AUM times a multi-year second count never overflows on the way through.
	/// Flooring is deliberate and always in the investor's favour: the residual sub-unit
	/// stays with the holder, never with the fee.
	pub fn scale(self, numer: u128, denom: u128) -> Result<Usdt, DomainError> {
		mul_div_floor(self.0, numer, denom)
			.map(Usdt)
			.ok_or_else(|| DomainError::Validation("amount scaling overflows or divides by zero".into()))
	}

	/// Parse a human/wire decimal string (e.g. `"100"`, `"100.5"`, `"0.000001"`)
	/// into canonical base units. Rejects more than 18 fractional digits and any
	/// non-digit. This is the inbound wire format (amounts cross gRPC as strings to
	/// stay exact in JS BFFs).
	pub fn parse_decimal(raw: &str) -> Result<Usdt, DomainError> {
		let trimmed = raw.trim();
		if trimmed.is_empty() {
			return Err(DomainError::Validation("amount must not be empty".into()));
		}
		let (int_part, frac_part) = match trimmed.split_once('.') {
			Some((i, f)) => (i, f),
			None => (trimmed, ""),
		};
		if int_part.is_empty() && frac_part.is_empty() {
			return Err(DomainError::Validation("amount must have digits".into()));
		}
		if frac_part.len() as u32 > CANONICAL_DECIMALS {
			return Err(DomainError::Validation("amount has more than 18 decimal places".into()));
		}
		if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
			return Err(DomainError::Validation("amount must be a decimal number".into()));
		}
		let int_units = int_part.parse::<u128>().map_err(|_| DomainError::Validation("amount integer part too large".into()))?;
		let scaled_int = int_units
			.checked_mul(10u128.pow(CANONICAL_DECIMALS))
			.ok_or_else(|| DomainError::Validation("amount too large".into()))?;
		// Right-pad the fraction to 18 digits, then it is already a base-unit count.
		let mut frac_units = 0u128;
		if !frac_part.is_empty() {
			let padded = format!("{frac_part:0<18}");
			frac_units = padded.parse::<u128>().map_err(|_| DomainError::Validation("invalid amount fraction".into()))?;
		}
		scaled_int.checked_add(frac_units).map(Usdt).ok_or_else(|| DomainError::Validation("amount too large".into()))
	}

	/// Render as a fixed-point decimal string with trailing zeros trimmed (the
	/// outbound wire format). `"0"` for zero.
	pub fn to_decimal_string(self) -> String {
		let scale = 10u128.pow(CANONICAL_DECIMALS);
		let int = self.0 / scale;
		let frac = self.0 % scale;
		if frac == 0 {
			return int.to_string();
		}
		let frac = format!("{frac:018}");
		let frac = frac.trim_end_matches('0');
		format!("{int}.{frac}")
	}
}

impl Serialize for Usdt {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.0.to_string())
	}
}

impl<'de> Deserialize<'de> for Usdt {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let raw = String::deserialize(deserializer)?;
		raw.parse::<u128>().map(Usdt).map_err(serde::de::Error::custom)
	}
}

impl core::fmt::Display for Usdt {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.to_decimal_string())
	}
}

/// A fund-unit (share) count in canonical 18-decimal base units — the **service
/// currency**. A holder's cash value is `units × NAV`, never the unit count itself,
/// so profit shows up as a rising [`Nav`], not as more units. Lives on its own
/// TigerBeetle ledger (`Ledger::Share`); deliberately a distinct newtype from [`Usdt`]
/// so a unit count can never be added to or confused with a cash amount. Fractional
/// at 18 dp, so `cash / NAV` floors to ≤ 1e-18 share of dust — no whole-share rounding.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Shares(u128);
impl Shares {
	pub const ZERO: Shares = Shares(0);

	pub const fn from_base_units(units: u128) -> Self {
		Self(units)
	}

	pub const fn base_units(self) -> u128 {
		self.0
	}

	pub const fn is_zero(self) -> bool {
		self.0 == 0
	}

	pub fn checked_add(self, other: Shares) -> Option<Shares> {
		self.0.checked_add(other.0).map(Shares)
	}

	pub fn checked_sub(self, other: Shares) -> Option<Shares> {
		self.0.checked_sub(other.0).map(Shares)
	}

	/// Units minted for `cash` at `nav`: `floor(cash · 10^18 / nav)`. The caller
	/// rejects a zero result (cash too small for one base-unit of share); `nav` zero
	/// is a domain error (no price to deal at).
	pub fn from_cash(cash: Usdt, nav: Nav) -> Result<Shares, DomainError> {
		mul_div_floor(cash.base_units(), SCALE, nav.base_units())
			.map(Shares)
			.ok_or_else(|| DomainError::Validation("cannot price shares (nav is zero or amount overflows)".into()))
	}

	pub fn parse_decimal(raw: &str) -> Result<Shares, DomainError> {
		parse_fixed_point(raw).map(Shares)
	}

	pub fn to_decimal_string(self) -> String {
		render_fixed_point(self.0)
	}
}

/// NAV — the price of one whole share, in canonical 18-decimal USDT. Derived
/// (`AUM / units_outstanding`), never stored in TigerBeetle (it is a price, not a
/// balance). A distinct newtype so a price can't be mistaken for a cash amount.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Nav(u128);
impl Nav {
	/// The bootstrap price used for the first subscription into a fund (when no units
	/// are outstanding yet): 1.0 USDT per share. The first operator mark re-prices.
	pub const SEED: Nav = Nav(SCALE);

	pub const fn from_base_units(units: u128) -> Self {
		Self(units)
	}

	pub const fn base_units(self) -> u128 {
		self.0
	}

	pub const fn is_zero(self) -> bool {
		self.0 == 0
	}

	/// Derive NAV from a posted fund AUM and the units outstanding:
	/// `floor(aum · 10^18 / units)`. Errors if `units` is zero (NAV undefined).
	pub fn from_aum(aum: Usdt, units: Shares) -> Result<Nav, DomainError> {
		if units.is_zero() {
			return Err(DomainError::Validation("nav undefined: no units outstanding".into()));
		}
		mul_div_floor(aum.base_units(), SCALE, units.base_units())
			.map(Nav)
			.ok_or_else(|| DomainError::Validation("nav overflows".into()))
	}

	/// The cash value of `units` at this NAV: `floor(units · nav / 10^18)`. The ≤1
	/// base-unit floor residual on a redemption stays in the fund's claim.
	pub fn value(self, units: Shares) -> Result<Usdt, DomainError> {
		mul_div_floor(units.base_units(), self.0, SCALE)
			.map(Usdt::from_base_units)
			.ok_or_else(|| DomainError::Validation("share value overflows".into()))
	}

	/// Checked sub — `None` below zero. Prices subtract when measuring a *gain per
	/// unit* against a high-water mark, which is where the `None` (a fund under its
	/// mark) is the whole point: no gain, so no performance fee.
	pub fn checked_sub(self, other: Nav) -> Option<Nav> {
		self.0.checked_sub(other.0).map(Nav)
	}

	/// `floor(self · numer / denom)` — the price-side twin of [`Usdt::scale`], used to
	/// grow a high-water mark by a hurdle rate over an elapsed fraction of a year.
	pub fn scale(self, numer: u128, denom: u128) -> Result<Nav, DomainError> {
		mul_div_floor(self.0, numer, denom)
			.map(Nav)
			.ok_or_else(|| DomainError::Validation("nav scaling overflows or divides by zero".into()))
	}

	/// Checked add — `None` on overflow.
	pub fn checked_add(self, other: Nav) -> Option<Nav> {
		self.0.checked_add(other.0).map(Nav)
	}

	pub fn parse_decimal(raw: &str) -> Result<Nav, DomainError> {
		parse_fixed_point(raw).map(Nav)
	}

	pub fn to_decimal_string(self) -> String {
		render_fixed_point(self.0)
	}
}

/// Whether `value` carries a URL by the needles concierge's governance relay refuses
/// (`no_url`): a scheme separator or a `www.` host, case-insensitive. Text that ends up in
/// a payment label has to clear exactly these, or its mail is never sent.
pub(crate) fn contains_url(value: &str) -> bool {
	let lower = value.to_ascii_lowercase();
	lower.contains("://") || lower.contains("www.")
}

/// Full 256-bit product of two `u128`s as `(hi, lo)`. The NAV math multiplies an
/// 18-dp amount by `10^18` before dividing, which overflows `u128` for any amount
/// above ~340 USDT — so the intermediate product must be 256-bit, never `u128`.
const fn widening_mul(a: u128, b: u128) -> (u128, u128) {
	let mask = u64::MAX as u128;
	let (a_lo, a_hi) = (a & mask, a >> 64);
	let (b_lo, b_hi) = (b & mask, b >> 64);
	let p0 = a_lo * b_lo;
	let p1 = a_lo * b_hi;
	let p2 = a_hi * b_lo;
	let p3 = a_hi * b_hi;
	let mut lo = p0;
	let mut hi = p3;
	let (s1, c1) = lo.overflowing_add(p1 << 64);
	lo = s1;
	hi += (p1 >> 64) + c1 as u128;
	let (s2, c2) = lo.overflowing_add(p2 << 64);
	lo = s2;
	hi += (p2 >> 64) + c2 as u128;
	(hi, lo)
}

/// Floor of `(a * b) / denom` with a 256-bit intermediate, via binary long division.
/// `None` on `denom == 0` or when the quotient would exceed `u128::MAX` (`hi >= denom`).
/// Money math is rare, so the O(128) loop is irrelevant; correctness is the point.
pub(crate) fn mul_div_floor(a: u128, b: u128, denom: u128) -> Option<u128> {
	if denom == 0 {
		return None;
	}
	if let Some(prod) = a.checked_mul(b) {
		return Some(prod / denom);
	}
	let (hi, lo) = widening_mul(a, b);
	if hi >= denom {
		return None;
	}
	let mut rem = hi;
	let mut quo = 0u128;
	let mut bit: u32 = 128;
	while bit > 0 {
		bit -= 1;
		let carry_in = (lo >> bit) & 1;
		let top = rem >> 127;
		let shifted = (rem << 1) | carry_in;
		if top == 1 || shifted >= denom {
			quo |= 1u128 << bit;
			rem = shifted.wrapping_sub(denom);
		} else {
			rem = shifted;
		}
	}
	Some(quo)
}

/// Parse a human/wire decimal string into 18-dp base units (shared by [`Shares`] and
/// [`Nav`]; mirrors [`Usdt::parse_decimal`]).
fn parse_fixed_point(raw: &str) -> Result<u128, DomainError> {
	Usdt::parse_decimal(raw).map(Usdt::base_units)
}

/// Render 18-dp base units as a trimmed decimal string (shared by [`Shares`]/[`Nav`]).
fn render_fixed_point(units: u128) -> String {
	Usdt::from_base_units(units).to_decimal_string()
}

macro_rules! impl_fixed_point_serde {
	($ty:ty) => {
		impl Serialize for $ty {
			fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
				serializer.serialize_str(&self.0.to_string())
			}
		}
		impl<'de> Deserialize<'de> for $ty {
			fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
				let raw = String::deserialize(deserializer)?;
				raw.parse::<u128>().map(Self).map_err(serde::de::Error::custom)
			}
		}
		impl core::fmt::Display for $ty {
			fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
				f.write_str(&self.to_decimal_string())
			}
		}
	};
}
impl_fixed_point_serde!(Shares);
impl_fixed_point_serde!(Nav);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn checked_arithmetic_does_not_wrap() {
		assert_eq!(Usdt::from_base_units(3).checked_sub(Usdt::from_base_units(5)), None);
		assert_eq!(Usdt::from_base_units(u128::MAX).checked_add(Usdt::from_base_units(1)), None);
		assert_eq!(Usdt::from_base_units(5).checked_sub(Usdt::from_base_units(2)), Some(Usdt::from_base_units(3)));
	}

	#[test]
	fn decimal_parsing_and_rendering() {
		assert_eq!(Usdt::parse_decimal("100").unwrap().base_units(), 100_000_000_000_000_000_000);
		assert_eq!(Usdt::parse_decimal("100.5").unwrap().base_units(), 100_500_000_000_000_000_000);
		assert_eq!(Usdt::parse_decimal("0.000000000000000001").unwrap().base_units(), 1);
		assert_eq!(Usdt::from_base_units(100_500_000_000_000_000_000).to_decimal_string(), "100.5");
		assert_eq!(Usdt::from_base_units(0).to_decimal_string(), "0");
		assert_eq!(Usdt::from_base_units(1).to_decimal_string(), "0.000000000000000001");
		// More than 18 fractional digits is rejected, not truncated.
		assert!(Usdt::parse_decimal("0.0000000000000000001").is_err());
		assert!(Usdt::parse_decimal("abc").is_err());
		assert!(Usdt::parse_decimal("").is_err());
	}

	#[test]
	fn usdt_serializes_as_base_unit_string() {
		let json = serde_json::to_string(&Usdt::from_base_units(42)).unwrap();
		assert_eq!(json, r#""42""#);
		let back: Usdt = serde_json::from_str(r#""42""#).unwrap();
		assert_eq!(back, Usdt::from_base_units(42));
	}

	#[test]
	fn mul_div_floor_matches_u128_on_the_fast_path() {
		assert_eq!(mul_div_floor(10, 3, 4), Some(7)); // 30/4 = 7.5 → 7
		assert_eq!(mul_div_floor(0, 12345, 7), Some(0));
		assert_eq!(mul_div_floor(5, 5, 0), None);
	}

	#[test]
	fn mul_div_floor_uses_the_256bit_path_past_u128() {
		// a*b overflows u128 (each ~2^120), but the quotient fits. Cross-check against
		// the exact value computed with the same widening multiply.
		let a = 1u128 << 120;
		let b = 1u128 << 100; // a*b = 2^220, overflows u128
		// 2^220 / 2^64 = 2^156 — exceeds u128 → None.
		assert_eq!(mul_div_floor(a, b, 1u128 << 64), None);
		// 2^220 / 2^100 = 2^120 — fits.
		assert_eq!(mul_div_floor(a, b, 1u128 << 100), Some(1u128 << 120));
		// 2^220 / 2^127 = 2^93 — fits (max representable denom).
		assert_eq!(mul_div_floor(a, b, 1u128 << 127), Some(1u128 << 93));
	}

	#[test]
	fn shares_from_cash_overflows_naive_mul_but_not_mul_div() {
		// 1000 USDT · 10^18 overflows u128 (the ~340 USDT boundary), so a naive
		// checked_mul path would wrong-reject this everyday subscription.
		let cash = Usdt::parse_decimal("1000").unwrap();
		let nav = Nav::parse_decimal("2").unwrap();
		// 1000 / 2 = 500 shares.
		assert_eq!(Shares::from_cash(cash, nav).unwrap(), Shares::parse_decimal("500").unwrap());
	}

	#[test]
	fn nav_derivation_and_valuation_round_trip() {
		// AUM 300 over 200 units → NAV 1.5; 200 units valued at 1.5 → 300.
		let aum = Usdt::parse_decimal("300").unwrap();
		let units = Shares::parse_decimal("200").unwrap();
		let nav = Nav::from_aum(aum, units).unwrap();
		assert_eq!(nav, Nav::parse_decimal("1.5").unwrap());
		assert_eq!(nav.value(units).unwrap(), aum);
		// NAV is undefined with no units outstanding.
		assert!(Nav::from_aum(aum, Shares::ZERO).is_err());
	}

	#[test]
	fn shares_dust_floors_to_zero_only_below_one_base_unit() {
		// At NAV 3, the smallest cash buying ≥ 1e-18 share is 3 base units of USDT
		// (3e-18 USDT / 3 = 1e-18 share); 2 base units floors to zero.
		let nav = Nav::parse_decimal("3").unwrap();
		assert!(Shares::from_cash(Usdt::from_base_units(2), nav).unwrap().is_zero());
		assert_eq!(Shares::from_cash(Usdt::from_base_units(3), nav).unwrap(), Shares::from_base_units(1));
	}

	#[test]
	fn scale_pro_rates_without_overflowing_the_intermediate() {
		// 2% of a year on 1000 USDT: 1000 · 200 · 31_536_000 / (10_000 · 31_536_000) = 20.
		let year: u128 = 365 * 24 * 60 * 60;
		let principal = Usdt::parse_decimal("1000").unwrap();
		assert_eq!(principal.scale(200 * year, 10_000 * year).unwrap(), Usdt::parse_decimal("20").unwrap());
		// Half a year is half the fee — the numerator alone (1000e18 · 200 · 15768000)
		// overflows u128, so this only holds on the 256-bit path.
		assert_eq!(principal.scale(200 * (year / 2), 10_000 * year).unwrap(), Usdt::parse_decimal("10").unwrap());
		// Flooring keeps the residual with the holder, never with the fee.
		assert_eq!(Usdt::from_base_units(3).scale(1, 2).unwrap(), Usdt::from_base_units(1));
		assert!(principal.scale(1, 0).is_err());
	}

	#[test]
	fn nav_arithmetic_reports_no_gain_below_the_mark() {
		let mark = Nav::parse_decimal("1.5").unwrap();
		assert_eq!(Nav::parse_decimal("1.8").unwrap().checked_sub(mark), Some(Nav::parse_decimal("0.3").unwrap()));
		// Under the mark there is no gain at all — the performance fee's whole premise.
		assert_eq!(Nav::parse_decimal("1.2").unwrap().checked_sub(mark), None);
		// A 5% hurdle over a full year lifts the mark to 1.575.
		let year: u128 = 365 * 24 * 60 * 60;
		let hurdle = mark.scale(500 * year, 10_000 * year).unwrap();
		assert_eq!(mark.checked_add(hurdle).unwrap(), Nav::parse_decimal("1.575").unwrap());
	}

	#[test]
	fn shares_and_nav_serialize_as_base_unit_strings() {
		assert_eq!(serde_json::to_string(&Shares::from_base_units(42)).unwrap(), r#""42""#);
		assert_eq!(serde_json::to_string(&Nav::from_base_units(7)).unwrap(), r#""7""#);
		assert_eq!(serde_json::from_str::<Shares>(r#""42""#).unwrap(), Shares::from_base_units(42));
		assert_eq!(Shares::parse_decimal("1.5").unwrap().to_decimal_string(), "1.5");
		assert_eq!(Nav::SEED.to_decimal_string(), "1");
	}
}
