use bitcoin::Amount;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usd(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Leverage(u8);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradingPair {
    BtcUsd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Position {
    Buy,
    Sell,
}

/// A concrete offer for a user
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfdOffer {
    pub id: Uuid,

    pub price: Usd,

    // TODO: [post-MVP] Representation of the contract size; at the moment the contract size is always 1 USD
    pub min_amount: Usd,
    pub max_amount: Usd,

    // TODO: [post-MVP] Allow different values
    pub leverage: Leverage,
    pub trading_pair: TradingPair,

    // TODO: [post-MVP] This is dynamic based on the price and leverage (might want to created packages for this or calculate on UI side)
    pub liquidation_price: Usd,
}

/// The taker POSTs this to create a Cfd
#[derive(Debug, Clone, Deserialize)]
pub struct CfdTakeRequest {
    pub offer_id: Uuid,
    pub quantity: Usd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Error {
    // TODO
    ConnectionLost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfdStateError {
    last_successful_state: CfdState,
    error: Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CfdState {
    /// The taker has sent an open request
    TakeRequested,
    /// We have sent a request to the maker to open the CFD but don't have a response yet
    PendingOpen,
    /// The maker has agreed to the CFD, but the contract is not set up on chain yet
    Accepted,
    /// The CFD contract is set up on chain
    Open,
    /// The taker has requested to close the position, but we have not passed that on to the blockchain yet.
    CloseRequested,
    /// The close transaction (CET) was published on the Bitcoin blockchain but we don't have a confirmation yet.
    PendingClose,
    /// The close transaction is confirmed with at least one block
    Closed,
    // FIXME: add CfdError
    Error,
}

/// Represents a cfd (including state)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cfd {
    pub initial_price: Usd,

    pub leverage: Leverage,
    pub trading_pair: TradingPair,
    pub liquidation_price: Usd,

    #[serde(with = "::bitcoin::util::amount::serde::as_btc")]
    pub quantity_btc: Amount,
    pub quantity_usd: Usd,

    #[serde(with = "::bitcoin::util::amount::serde::as_btc")]
    pub profit_btc: Amount,
    pub profit_usd: Usd,

    pub creation_date: SystemTime,

    pub state: CfdState,
}

pub fn static_cfd_offer() -> CfdOffer {
    CfdOffer {
        id: Uuid::new_v4(),
        price: Usd(49_000),
        min_amount: Usd(1_000),
        max_amount: Usd(10_000),
        leverage: Leverage(5),
        trading_pair: TradingPair::BtcUsd,
        liquidation_price: Usd(42_000),
    }
}
