use crate::model::cfd::CfdOfferId;
use crate::model::Usd;
use crate::CfdOffer;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::{Address, Amount, PublicKey};
use cfd_protocol::{PartyParams, PunishParams};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
#[allow(clippy::large_enum_variant)]
pub enum TakerToMaker {
    TakeOffer { offer_id: CfdOfferId, quantity: Usd },
    // TODO: Currently the taker starts, can already send some stuff for signing over in the first message.
    StartContractSetup(CfdOfferId),
    Protocol(SetupMsg),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
#[allow(clippy::large_enum_variant)]
pub enum MakerToTaker {
    CurrentOffer(Option<CfdOffer>),
    // TODO: Needs RejectOffer as well
    ConfirmTakeOffer(CfdOfferId), // TODO: Include payout curve in "accept" message from maker
    InvalidOfferId(CfdOfferId),
    SetupContract(SetupMsg),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum SetupMsg {
    Msg0(Msg0),
}

impl SetupMsg {
    pub fn try_into_msg0(self) -> Result<Msg0, Self> {
        if let Self::Msg0(v) = self {
            Ok(v)
        } else {
            Err(self)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Msg0 {
    pub lock_psbt: PartiallySignedTransaction,
    pub identity_pk: PublicKey,
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    pub lock_amount: Amount,
    pub address: Address,
    pub revocation_pk: PublicKey,
    pub publish_pk: PublicKey,
}

impl From<(PartyParams, PunishParams)> for Msg0 {
    fn from((party, punish): (PartyParams, PunishParams)) -> Self {
        let PartyParams {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
        } = party;
        let PunishParams {
            revocation_pk,
            publish_pk,
        } = punish;

        Self {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
            revocation_pk,
            publish_pk,
        }
    }
}

impl From<Msg0> for (PartyParams, PunishParams) {
    fn from(msg0: Msg0) -> Self {
        let Msg0 {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
            revocation_pk,
            publish_pk,
        } = msg0;

        let party = PartyParams {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
        };
        let punish = PunishParams {
            revocation_pk,
            publish_pk,
        };

        (party, punish)
    }
}
