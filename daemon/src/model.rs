use crate::impl_sqlx_type_display_from_str;
use crate::model::cfd::calculate_long_margin;
use crate::model::cfd::calculate_short_margin;
use crate::olivia;
use crate::SETTLEMENT_INTERVAL;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::Address;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Denomination;
use bdk::bitcoin::SignedAmount;
use chrono::DateTime;
use parse_display::Display;
use reqwest::Url;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::de::Error as _;
use serde::Deserialize;
use serde::Serialize;
use serde_with::DeserializeFromStr;
use serde_with::SerializeDisplay;
use std::convert::TryInto;
use std::fmt;
use std::num::NonZeroU8;
use std::ops::Add;
use std::ops::Div;
use std::ops::Mul;
use std::ops::Sub;
use std::str;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use time::OffsetDateTime;
use time::PrimitiveDateTime;
use time::Time;

use self::cfd::Role;

pub mod cfd;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Price of zero is not allowed.")]
    ZeroPrice,
    #[error("Negative Price is unimplemented.")]
    NegativePrice,
}

/// Represents "quantity" or "contract size" in Cfd terms
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Usd(Decimal);

impl Usd {
    pub fn new(value: Decimal) -> Self {
        Self(value)
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("could not fit decimal into u64")
    }

    #[must_use]
    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.round_dp(2).fmt(f)
    }
}

impl str::FromStr for Usd {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(Usd(dec))
    }
}

impl_sqlx_type_display_from_str!(Usd);

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Price(Decimal);

impl_sqlx_type_display_from_str!(Price);

impl Price {
    pub fn new(value: Decimal) -> Result<Self, Error> {
        if value == Decimal::ZERO {
            return Result::Err(Error::ZeroPrice);
        }

        if value < Decimal::ZERO {
            return Result::Err(Error::NegativePrice);
        }

        Ok(Self(value))
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("Could not fit decimal into u64")
    }

    pub fn try_into_f64(&self) -> Result<f64> {
        self.0.to_f64().context("Could not fit decimal into f64")
    }

    #[must_use]
    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for Price {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(Price(dec))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InversePrice(Decimal);

impl InversePrice {
    pub fn new(value: Price) -> Result<Self, Error> {
        if value.0 == Decimal::ZERO {
            return Result::Err(Error::ZeroPrice);
        }

        if value.0 < Decimal::ZERO {
            return Result::Err(Error::NegativePrice);
        }

        Ok(Self(Decimal::ONE / value.0))
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("Could not fit decimal into u64")
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Leverage(u8);

impl Leverage {
    pub fn new(value: u8) -> Result<Self> {
        let val = NonZeroU8::new(value).context("Cannot use non-positive values")?;
        Ok(Self(u8::from(val)))
    }

    pub fn get(&self) -> u8 {
        self.0
    }
}

impl fmt::Display for Leverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let leverage = self.0;

        write!(f, "x{leverage}")
    }
}

// add impl's to do algebra with Usd, Leverage, and ExhangeRate as required
impl Mul<Leverage> for Usd {
    type Output = Usd;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Div<Leverage> for Usd {
    type Output = Usd;

    fn div(self, rhs: Leverage) -> Self::Output {
        Self(self.0 / Decimal::from(rhs.0))
    }
}

impl Mul<Usd> for Leverage {
    type Output = Usd;

    fn mul(self, rhs: Usd) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        Usd(value)
    }
}

impl Mul<u8> for Usd {
    type Output = Usd;

    fn mul(self, rhs: u8) -> Self::Output {
        let value = self.0 * Decimal::from(rhs);
        Self(value)
    }
}

impl Div<u8> for Usd {
    type Output = Usd;

    fn div(self, rhs: u8) -> Self::Output {
        let value = self.0 / Decimal::from(rhs);
        Self(value)
    }
}

impl Div<u8> for Price {
    type Output = Price;

    fn div(self, rhs: u8) -> Self::Output {
        let value = self.0 / Decimal::from(rhs);
        Self(value)
    }
}

impl Add<Usd> for Usd {
    type Output = Usd;

    fn add(self, rhs: Usd) -> Self::Output {
        let value = self.0 + rhs.0;
        Self(value)
    }
}

impl Sub<Usd> for Usd {
    type Output = Usd;

    fn sub(self, rhs: Usd) -> Self::Output {
        let value = self.0 - rhs.0;
        Self(value)
    }
}

impl Div<Price> for Usd {
    type Output = Amount;

    fn div(self, rhs: Price) -> Self::Output {
        let mut btc = self.0 / rhs.0;
        btc.rescale(8);
        Amount::from_str_in(&btc.to_string(), Denomination::Bitcoin)
            .expect("Error computing BTC amount")
    }
}

impl Mul<Leverage> for Price {
    type Output = Price;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<Price> for Leverage {
    type Output = Price;

    fn mul(self, rhs: Price) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        Price(value)
    }
}

impl Div<Leverage> for Price {
    type Output = Price;

    fn div(self, rhs: Leverage) -> Self::Output {
        let value = self.0 / Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<InversePrice> for Usd {
    type Output = Amount;

    fn mul(self, rhs: InversePrice) -> Self::Output {
        let mut btc = self.0 * rhs.0;
        btc.rescale(8);
        Amount::from_str_in(&btc.to_string(), Denomination::Bitcoin)
            .expect("Error computing BTC amount")
    }
}

impl Mul<Leverage> for InversePrice {
    type Output = InversePrice;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<InversePrice> for Leverage {
    type Output = InversePrice;

    fn mul(self, rhs: InversePrice) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        InversePrice(value)
    }
}

impl Div<Leverage> for InversePrice {
    type Output = InversePrice;

    fn div(self, rhs: Leverage) -> Self::Output {
        let value = self.0 / Decimal::from(rhs.0);
        Self(value)
    }
}

impl Add<Price> for Price {
    type Output = Price;

    fn add(self, rhs: Price) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub<Price> for Price {
    type Output = Price;

    fn sub(self, rhs: Price) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Add<InversePrice> for InversePrice {
    type Output = InversePrice;

    fn add(self, rhs: InversePrice) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub<InversePrice> for InversePrice {
    type Output = InversePrice;

    fn sub(self, rhs: InversePrice) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Add<u8> for Leverage {
    type Output = Leverage;

    fn add(self, rhs: u8) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl Add<Leverage> for u8 {
    type Output = Leverage;

    fn add(self, rhs: Leverage) -> Self::Output {
        Leverage(self + rhs.0)
    }
}

impl Div<Leverage> for Leverage {
    type Output = Decimal;

    fn div(self, rhs: Leverage) -> Self::Output {
        Decimal::from(self.0) / Decimal::from(rhs.0)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct Percent(Decimal);

impl Percent {
    #[must_use]
    pub fn round_dp(self, digits: u32) -> Self {
        Self(self.0.round_dp(digits))
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.round_dp(2).fmt(f)
    }
}

impl From<Decimal> for Percent {
    fn from(decimal: Decimal) -> Self {
        Percent(decimal)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, sqlx::Type)]
pub enum TradingPair {
    BtcUsd,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, sqlx::Type, Display)]
pub enum Position {
    Long,
    Short,
}

impl Position {
    /// Determines the counter position to the current position.
    pub fn counter_position(&self) -> Position {
        match self {
            Position::Long => Position::Short,
            Position::Short => Position::Long,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Identity(x25519_dalek::PublicKey);

impl Identity {
    pub fn new(key: x25519_dalek::PublicKey) -> Self {
        Self(key)
    }

    pub fn pk(&self) -> x25519_dalek::PublicKey {
        self.0
    }
}

impl Serialize for Identity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;

        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&hex, &mut bytes).map_err(D::Error::custom)?;

        Ok(Self(x25519_dalek::PublicKey::from(bytes)))
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = hex::encode(self.0.as_bytes());

        write!(f, "{hex}")
    }
}

impl str::FromStr for Identity {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut key = [0u8; 32];

        hex::decode_to_slice(s, &mut key)?;

        Ok(Self(key.into()))
    }
}

impl_sqlx_type_display_from_str!(Identity);

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub balance: Amount,
    pub address: Address,
    pub last_updated_at: Timestamp,
}

#[derive(
    Debug, Clone, Copy, SerializeDisplay, DeserializeFromStr, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct BitMexPriceEventId {
    /// The timestamp this price event refers to.
    timestamp: OffsetDateTime,
    digits: usize,
}

impl BitMexPriceEventId {
    pub fn new(timestamp: OffsetDateTime, digits: usize) -> Self {
        let (hours, minutes, seconds) = timestamp.time().as_hms();
        let time_without_nanos =
            Time::from_hms(hours, minutes, seconds).expect("original timestamp was valid");

        let timestamp_without_nanos = timestamp.replace_time(time_without_nanos);

        Self {
            timestamp: timestamp_without_nanos,
            digits,
        }
    }

    pub fn with_20_digits(timestamp: OffsetDateTime) -> Self {
        Self::new(timestamp, 20)
    }

    /// Checks whether this event has likely already occurred.
    ///
    /// We can't be sure about it because our local clock might be off from the oracle's clock.
    pub fn has_likely_occured(&self) -> bool {
        let now = OffsetDateTime::now_utc();

        now > self.timestamp
    }

    pub fn to_olivia_url(self) -> Url {
        "https://h00.ooo"
            .parse::<Url>()
            .expect("valid URL from constant")
            .join(&self.to_string())
            .expect("Event id can be joined")
    }

    pub fn timestamp(&self) -> OffsetDateTime {
        self.timestamp
    }
}

impl fmt::Display for BitMexPriceEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "/x/BitMEX/BXBT/{}.price?n={}",
            self.timestamp
                .format(&olivia::EVENT_TIME_FORMAT)
                .expect("should always format and we can't return an error here"),
            self.digits
        )
    }
}

impl str::FromStr for BitMexPriceEventId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let remaining = s.trim_start_matches("/x/BitMEX/BXBT/");
        let (timestamp, rest) = remaining.split_at(19);
        let digits = rest.trim_start_matches(".price?n=");

        Ok(Self {
            timestamp: PrimitiveDateTime::parse(timestamp, &olivia::EVENT_TIME_FORMAT)
                .with_context(|| format!("Failed to parse {timestamp} as timestamp"))?
                .assume_utc(),
            digits: digits.parse()?,
        })
    }
}

impl_sqlx_type_display_from_str!(BitMexPriceEventId);

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn new(seconds: i64) -> Self {
        Self(seconds)
    }

    pub fn now() -> Self {
        let seconds: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time not to go backwards")
            .as_secs()
            .try_into()
            .expect("seconds of system time to fit into i64");

        Self(seconds)
    }

    pub fn parse_from_rfc3339(datetime_str: &str) -> Result<Self> {
        let datetime = DateTime::parse_from_rfc3339(datetime_str)
            .context("Unable to parse datetime as RFC3339")?;
        let seconds = datetime.timestamp();

        Ok(Self(seconds))
    }

    pub fn seconds(&self) -> i64 {
        self.0
    }

    pub fn seconds_u64(&self) -> Result<u64> {
        let out = self.0.try_into().context("Unable to convert i64 to u64")?;
        Ok(out)
    }
}

/// Funding rate per SETTLEMENT_INTERVAL
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingRate(Decimal);

impl FundingRate {
    pub fn new(rate: Decimal) -> Result<Self> {
        anyhow::ensure!(
            rate.abs() <= Decimal::ONE,
            "Funding rate can't be higher than 100%"
        );

        Ok(Self(rate))
    }

    pub fn to_decimal(&self) -> Decimal {
        self.0
    }

    pub fn short_pays_long(&self) -> bool {
        self.0.is_sign_negative()
    }
}

impl Default for FundingRate {
    fn default() -> Self {
        Self::new(Decimal::ZERO).expect("hard-coded values to be valid")
    }
}

impl_sqlx_type_display_from_str!(FundingRate);

impl fmt::Display for FundingRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for FundingRate {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(FundingRate(dec))
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ConversionError {
    #[error("Underflow")]
    Underflow,
    #[error("Overflow")]
    Overflow,
}

/// Opening fee is always payed from taker to maker
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct OpeningFee {
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    fee: Amount,
}

impl OpeningFee {
    pub fn new(fee: Amount) -> Self {
        Self { fee }
    }

    pub fn to_inner(self) -> Amount {
        self.fee
    }
}

impl fmt::Display for OpeningFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fee.as_sat().fmt(f)
    }
}

impl str::FromStr for OpeningFee {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let amount_sat: u64 = s.parse()?;
        Ok(OpeningFee {
            fee: Amount::from_sat(amount_sat),
        })
    }
}

impl_sqlx_type_display_from_str!(OpeningFee);

impl Default for OpeningFee {
    fn default() -> Self {
        Self { fee: Amount::ZERO }
    }
}

/// Funding fee as defined by position and rate
///
/// Position and rate define if the funding rate is to be added or subtracted when summing up the
/// fees.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FundingFee {
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    fee: Amount,
    rate: FundingRate,
}

impl FundingFee {
    pub fn new(fee: Amount, rate: FundingRate) -> Self {
        Self { fee, rate }
    }

    pub fn to_inner(self) -> Amount {
        self.fee
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FeeFlow {
    LongPaysShort(Amount),
    ShortPaysLong(Amount),
    Nein,
}

/// Our own accumulated fees
///
/// The balance being positive means we owe this amount to the other party.
/// The balance being negative means that the other party owes this amount to us.
/// The counterparty fee-account balance is always the inverse of the balance.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FeeAccount {
    balance: SignedAmount,
    position: Position,
    role: Role,
}

impl FeeAccount {
    pub fn new(position: Position, role: Role) -> Self {
        Self {
            position,
            role,
            balance: SignedAmount::ZERO,
        }
    }

    pub fn settle(&self) -> FeeFlow {
        let absolute = self.balance.as_sat().unsigned_abs();
        let absolute = Amount::from_sat(absolute);

        if self.balance == SignedAmount::ZERO {
            FeeFlow::Nein
        } else if (self.position == Position::Long && self.balance.is_positive())
            || (self.position == Position::Short && self.balance.is_negative())
        {
            FeeFlow::LongPaysShort(absolute)
        } else {
            FeeFlow::ShortPaysLong(absolute)
        }
    }

    pub fn balance(&self) -> SignedAmount {
        self.balance
    }

    pub fn add_opening_fee(self, opening_fee: OpeningFee) -> Self {
        let fee: i64 = opening_fee
            .fee
            .as_sat()
            .try_into()
            .expect("not to overflow");

        let signed_fee = match self.role {
            Role::Maker => -fee,
            Role::Taker => fee,
        };

        let signed_fee = SignedAmount::from_sat(signed_fee);
        let sum = self.balance + signed_fee;

        Self {
            balance: sum,
            position: self.position,
            role: self.role,
        }
    }

    pub fn add_funding_fee(self, funding_fee: FundingFee) -> Self {
        let fee: i64 = funding_fee
            .fee
            .as_sat()
            .try_into()
            .expect("not to overflow");

        let signed_fee = if (self.position == Position::Long
            && funding_fee.rate.0.is_sign_positive())
            || (self.position == Position::Short && funding_fee.rate.0.is_sign_negative())
        {
            fee
        } else {
            -fee
        };

        let signed_fee = SignedAmount::from_sat(signed_fee);
        let sum = self.balance + signed_fee;

        Self {
            balance: sum,
            position: self.position,
            role: self.role,
        }
    }
}

pub fn calculate_funding_fee(
    price: Price,
    quantity: Usd,
    leverage: Leverage,
    funding_rate: FundingRate,
    hours_to_charge: i64,
) -> Result<FundingFee> {
    if funding_rate.0.is_zero() {
        return Ok(FundingFee::new(Amount::ZERO, funding_rate));
    }

    let margin = if funding_rate.short_pays_long() {
        calculate_long_margin(price, quantity, leverage)
    } else {
        calculate_short_margin(price, quantity)
    };

    let fraction_of_funding_period = if hours_to_charge as i64 == SETTLEMENT_INTERVAL.whole_hours()
    {
        Decimal::ONE
    } else {
        Decimal::from(hours_to_charge)
            .checked_div(Decimal::from(SETTLEMENT_INTERVAL.whole_hours()))
            .context("can't establish a fraction")?
    };

    let funding_fee = Decimal::from(margin.as_sat())
        * funding_rate.to_decimal().abs()
        * fraction_of_funding_period;
    let funding_fee = funding_fee
        .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::AwayFromZero)
        .to_u64()
        .context("Failed to represent as u64")?;

    Ok(FundingFee::new(Amount::from_sat(funding_fee), funding_rate))
}

/// Transaction fee in satoshis per vbyte
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct TxFeeRate(u32);

impl TxFeeRate {
    pub fn new(fee_rate: u32) -> Self {
        Self(fee_rate)
    }

    pub fn to_u32(self) -> u32 {
        self.0
    }
}

impl From<TxFeeRate> for bdk::FeeRate {
    fn from(fee_rate: TxFeeRate) -> Self {
        Self::from_sat_per_vb(fee_rate.to_u32() as f32)
    }
}

impl Default for TxFeeRate {
    fn default() -> Self {
        Self(1u32)
    }
}

impl fmt::Display for TxFeeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for TxFeeRate {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fee_sat = s.parse()?;
        Ok(TxFeeRate(fee_sat))
    }
}

impl_sqlx_type_display_from_str!(TxFeeRate);

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use time::macros::datetime;

    #[test]
    fn to_olivia_url() {
        let url = BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc())
            .to_olivia_url();

        assert_eq!(
            url,
            "https://h00.ooo/x/BitMEX/BXBT/2021-09-23T10:00:00.price?n=20"
                .parse()
                .unwrap()
        );
    }

    #[test]
    fn parse_event_id() {
        let parsed = "/x/BitMEX/BXBT/2021-09-23T10:00:00.price?n=20"
            .parse::<BitMexPriceEventId>()
            .unwrap();
        let expected =
            BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc());

        assert_eq!(parsed, expected);
    }

    #[test]
    fn new_event_has_no_nanos() {
        let now = BitMexPriceEventId::with_20_digits(OffsetDateTime::now_utc());

        assert_eq!(now.timestamp.nanosecond(), 0);
    }

    #[test]
    fn has_occured_if_in_the_past() {
        let past_event =
            BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc());

        assert!(past_event.has_likely_occured());
    }

    #[test]
    fn algebra_with_usd() {
        let usd_0 = Usd::new(dec!(1.234));
        let usd_1 = Usd::new(dec!(9.876));

        let usd_sum = usd_0 + usd_1;
        let usd_diff = usd_0 - usd_1;
        let half = usd_0 / 2;
        let double = usd_1 * 2;

        assert_eq!(usd_sum.0, dec!(11.110));
        assert_eq!(usd_diff.0, dec!(-8.642));
        assert_eq!(half.0, dec!(0.617));
        assert_eq!(double.0, dec!(19.752));
    }

    #[test]
    fn usd_for_1_btc_buys_1_btc() {
        let usd = Usd::new(dec!(61234.5678));
        let price = Price::new(dec!(61234.5678)).unwrap();
        let inv_price = InversePrice::new(price).unwrap();
        let res_0 = usd / price;
        let res_1 = usd * inv_price;

        assert_eq!(res_0, Amount::ONE_BTC);
        assert_eq!(res_1, Amount::ONE_BTC);
    }

    #[test]
    fn leverage_does_not_alter_type() {
        let usd = Usd::new(dec!(61234.5678));
        let leverage = Leverage::new(3).unwrap();
        let res = usd * leverage / leverage;

        assert_eq!(res.0, usd.0);
    }

    #[test]
    fn test_algebra_with_types() {
        let usd = Usd::new(dec!(61234.5678));
        let leverage = Leverage::new(5).unwrap();
        let price = Price::new(dec!(61234.5678)).unwrap();
        let expected_buyin = Amount::from_str_in("0.2", Denomination::Bitcoin).unwrap();

        let liquidation_price = price * leverage / (leverage + 1);
        let inv_price = InversePrice::new(price).unwrap();
        let inv_liquidation_price = InversePrice::new(liquidation_price).unwrap();

        let long_buyin = usd / (price * leverage);
        let long_payout =
            (usd / leverage) * ((leverage + 1) * inv_price - leverage * inv_liquidation_price);

        assert_eq!(long_buyin, expected_buyin);
        assert_eq!(long_payout, Amount::ZERO);
    }

    #[test]
    fn test_timestamp() {
        let datetime_str_a = "1999-12-31T23:59:00.00Z";
        let datetime_str_b = "1999-12-31T23:59:00.00+10:00";
        let ts_a = Timestamp::parse_from_rfc3339(datetime_str_a).unwrap();
        let ts_b = Timestamp::parse_from_rfc3339(datetime_str_b).unwrap();

        assert_eq!(ts_b.seconds() - ts_a.seconds(), -36000);
    }

    #[test]
    fn roundtrip_identity_serde() {
        let id = Identity::new(x25519_dalek::PublicKey::from([42u8; 32]));

        serde_test::assert_tokens(
            &id,
            &[serde_test::Token::String(
                "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a",
            )],
        );
    }

    #[test]
    fn long_taker_pays_opening_fee_to_maker() {
        let opening_fee = OpeningFee::new(Amount::from_sat(500));

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_opening_fee(opening_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_opening_fee(opening_fee)
            .settle();

        assert_eq!(long_taker, FeeFlow::LongPaysShort(Amount::from_sat(500)));
        assert_eq!(short_maker, FeeFlow::LongPaysShort(Amount::from_sat(500)));
    }

    #[test]
    fn short_taker_pays_opening_fee_to_maker() {
        let opening_fee = OpeningFee::new(Amount::from_sat(500));

        let short_taker = FeeAccount::new(Position::Short, Role::Taker)
            .add_opening_fee(opening_fee)
            .settle();
        let long_maker = FeeAccount::new(Position::Long, Role::Maker)
            .add_opening_fee(opening_fee)
            .settle();

        assert_eq!(short_taker, FeeFlow::ShortPaysLong(Amount::from_sat(500)));
        assert_eq!(long_maker, FeeFlow::ShortPaysLong(Amount::from_sat(500)));
    }

    #[test]
    fn long_pays_short_with_positive_funding_rate() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();

        assert_eq!(long_taker, FeeFlow::LongPaysShort(Amount::from_sat(1000)));
        assert_eq!(short_maker, FeeFlow::LongPaysShort(Amount::from_sat(1000)));
    }

    #[test]
    fn fee_account_handles_balance_of_zero() {
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee_with_positive_rate)
            .add_funding_fee(funding_fee_with_negative_rate)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee_with_positive_rate)
            .add_funding_fee(funding_fee_with_negative_rate)
            .settle();

        assert_eq!(long_taker, FeeFlow::Nein);
        assert_eq!(short_maker, FeeFlow::Nein);
    }

    #[test]
    fn fee_account_handles_negative_funding_rate() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();

        assert_eq!(long_taker, FeeFlow::ShortPaysLong(Amount::from_sat(1000)));
        assert_eq!(short_maker, FeeFlow::ShortPaysLong(Amount::from_sat(1000)));
    }

    #[test]
    fn long_taker_short_maker_roundtrip() {
        let opening_fee = OpeningFee::new(Amount::from_sat(100));
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);

        assert_eq!(
            long_taker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(600))
        );
        assert_eq!(
            short_maker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(600))
        );

        let long_taker = long_taker.add_funding_fee(funding_fee_with_negative_rate);
        let short_maker = short_maker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_taker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(100))
        );
        assert_eq!(
            short_maker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(100))
        );

        let long_taker = long_taker.add_funding_fee(funding_fee_with_negative_rate);
        let short_maker = short_maker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_taker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(400))
        );
        assert_eq!(
            short_maker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(400))
        );
    }

    #[test]
    fn long_maker_short_taker_roundtrip() {
        let opening_fee = OpeningFee::new(Amount::from_sat(100));
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_maker = FeeAccount::new(Position::Long, Role::Maker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);
        let short_taker = FeeAccount::new(Position::Short, Role::Taker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);

        assert_eq!(
            long_maker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(400))
        );
        assert_eq!(
            short_taker.settle(),
            FeeFlow::LongPaysShort(Amount::from_sat(400))
        );

        let long_maker = long_maker.add_funding_fee(funding_fee_with_negative_rate);
        let short_taker = short_taker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_maker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(100))
        );
        assert_eq!(
            short_taker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(100))
        );

        let long_maker = long_maker.add_funding_fee(funding_fee_with_negative_rate);
        let short_taker = short_taker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_maker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(600))
        );
        assert_eq!(
            short_taker.settle(),
            FeeFlow::ShortPaysLong(Amount::from_sat(600))
        );
    }

    #[test]
    fn given_positive_rate_then_positive_taker_long_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(1000))
    }

    #[test]
    fn given_positive_rate_then_negative_maker_short_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(-1000))
    }

    #[test]
    fn given_negative_rate_then_negative_taker_long_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(-1000))
    }

    #[test]
    fn given_negative_rate_then_positive_maker_short_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(1000))
    }

    #[test]
    fn proportional_funding_fees_if_sign_of_funding_rate_changes() {
        let long_leverage = Leverage::new(2).unwrap();

        let funding_rate_pos = FundingRate::new(dec!(0.01)).unwrap();
        let long_pays_short_fee = calculate_funding_fee(
            dummy_price(),
            dummy_n_contracts(),
            long_leverage,
            funding_rate_pos,
            dummy_settlement_interval(),
        )
        .unwrap();

        let funding_rate_neg = FundingRate::new(dec!(-0.01)).unwrap();
        let short_pays_long_fee = calculate_funding_fee(
            dummy_price(),
            dummy_n_contracts(),
            long_leverage,
            funding_rate_neg,
            dummy_settlement_interval(),
        )
        .unwrap();

        let epsilon = (long_pays_short_fee.fee.as_sat() as i64)
            - (short_pays_long_fee.fee.as_sat() as i64) * (long_leverage.get() as i64);
        assert!(epsilon.abs() < 5)
    }

    #[test]
    fn zero_funding_fee_if_zero_funding_rate() {
        let zero_funding_rate = FundingRate::new(Decimal::ZERO).unwrap();

        let dummy_leverage = Leverage::new(1).unwrap();
        let fee = calculate_funding_fee(
            dummy_price(),
            dummy_n_contracts(),
            dummy_leverage,
            zero_funding_rate,
            dummy_settlement_interval(),
        )
        .unwrap();

        assert_eq!(fee.fee, Amount::ZERO)
    }

    fn dummy_price() -> Price {
        Price::new(dec!(35_000)).expect("to not fail")
    }

    fn dummy_n_contracts() -> Usd {
        Usd::new(dec!(100))
    }

    fn dummy_settlement_interval() -> i64 {
        8
    }
}
