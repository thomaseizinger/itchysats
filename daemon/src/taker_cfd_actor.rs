use crate::model::cfd::{Cfd, CfdOffer, CfdOfferId, CfdState, CfdStateCommon};
use crate::model::Usd;
use crate::wire::{Msg0, SetupMsg};
use crate::{db, wire};
use bdk::bitcoin;
use bdk::bitcoin::secp256k1::{schnorrsig, SecretKey};
use bdk::database::BatchDatabase;
use cfd_protocol::{create_cfd_transactions, OracleParams, PartyParams, PunishParams, WalletExt};
use core::panic;
use futures::Future;
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    TakeOffer { offer_id: CfdOfferId, quantity: Usd },
    NewOffer(Option<CfdOffer>),
    OfferAccepted(CfdOfferId),
    IncProtocolMsg(SetupMsg),
    CfdSetupCompleted(FinalizedCfd),
}

pub fn new<B, D>(
    db: sqlx::SqlitePool,
    wallet: bdk::Wallet<B, D>,
    oracle_pk: schnorrsig::PublicKey,
    cfd_feed_actor_inbox: watch::Sender<Vec<Cfd>>,
    offer_feed_actor_inbox: watch::Sender<Option<CfdOffer>>,
    out_msg_maker_inbox: mpsc::UnboundedSender<wire::TakerToMaker>,
) -> (impl Future<Output = ()>, mpsc::UnboundedSender<Command>)
where
    D: BatchDatabase,
{
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut current_contract_setup = None;

    let actor = {
        let sender = sender.clone();

        async move {
            while let Some(message) = receiver.recv().await {
                match message {
                    Command::TakeOffer { offer_id, quantity } => {
                        let mut conn = db.acquire().await.unwrap();

                        let current_offer =
                            db::load_offer_by_id(offer_id, &mut conn).await.unwrap();

                        println!("Accepting current offer: {:?}", &current_offer);

                        let cfd = Cfd::new(
                            current_offer,
                            quantity,
                            CfdState::PendingTakeRequest {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                            Usd::ZERO,
                        )
                        .unwrap();

                        db::insert_cfd(cfd, &mut conn).await.unwrap();

                        cfd_feed_actor_inbox
                            .send(db::load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();
                        out_msg_maker_inbox
                            .send(wire::TakerToMaker::TakeOffer { offer_id, quantity })
                            .unwrap();
                    }
                    Command::NewOffer(Some(offer)) => {
                        let mut conn = db.acquire().await.unwrap();
                        db::insert_cfd_offer(&offer, &mut conn).await.unwrap();
                        offer_feed_actor_inbox.send(Some(offer)).unwrap();
                    }
                    Command::NewOffer(None) => {
                        offer_feed_actor_inbox.send(None).unwrap();
                    }
                    Command::OfferAccepted(offer_id) => {
                        let mut conn = db.acquire().await.unwrap();
                        db::insert_new_cfd_state_by_offer_id(
                            offer_id,
                            CfdState::ContractSetup {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                            &mut conn,
                        )
                        .await
                        .unwrap();

                        cfd_feed_actor_inbox
                            .send(db::load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();

                        let (sk, pk) = crate::keypair::new(&mut rand::thread_rng());

                        let taker_params = wallet
                            .build_party_params(bitcoin::Amount::ZERO, pk) // TODO: Load correct quantity from DB
                            .unwrap();

                        let (actor, inbox) = setup_contract(
                            {
                                let inbox = out_msg_maker_inbox.clone();

                                move |msg| inbox.send(wire::TakerToMaker::Protocol(msg)).unwrap()
                            },
                            {
                                let sender = sender.clone();

                                move |finalized_cfd| {
                                    sender
                                        .send(Command::CfdSetupCompleted(finalized_cfd))
                                        .unwrap()
                                }
                            },
                            taker_params,
                            sk,
                            oracle_pk,
                        );

                        tokio::spawn(actor);
                        current_contract_setup = Some(inbox);
                    }
                    Command::IncProtocolMsg(msg) => {
                        let inbox = match &current_contract_setup {
                            None => panic!("whoops"),
                            Some(inbox) => inbox,
                        };

                        inbox.send(msg).unwrap();
                    }
                    Command::CfdSetupCompleted(finalized_cfd) => {
                        todo!("but what?")
                    }
                }
            }
        }
    };

    (actor, sender)
}

#[derive(Debug)]
pub struct FinalizedCfd {}

fn setup_contract(
    send_to_maker: impl Fn(SetupMsg),
    completed: impl Fn(FinalizedCfd),
    taker_params: PartyParams,
    sk: SecretKey,
    oracle_pk: schnorrsig::PublicKey,
) -> (
    impl Future<Output = FinalizedCfd>,
    mpsc::UnboundedSender<SetupMsg>,
) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<SetupMsg>();

    let actor = async move {
        let (_rev_sk, rev_pk) = crate::keypair::new(&mut rand::thread_rng());
        let (_publish_sk, publish_pk) = crate::keypair::new(&mut rand::thread_rng());

        let taker_punish_params = PunishParams {
            revocation_pk: rev_pk,
            publish_pk,
        };
        send_to_maker(SetupMsg::Msg0(Msg0::from((
            taker_params.clone(),
            taker_punish_params,
        ))));

        let msg0 = receiver.recv().await.unwrap().try_into_msg0().unwrap();
        let (maker_params, maker_punish_params) = msg0.into();

        let _taker_cfd_txs = create_cfd_transactions(
            (maker_params, maker_punish_params),
            (taker_params, taker_punish_params),
            OracleParams {
                pk: oracle_pk,
                nonce_pk: todo!("query oracle"),
            },
            0, // TODO: Calculate refund timelock based on CFD term
            vec![],
            sk,
        )
        .unwrap();

        completed(FinalizedCfd {});
    };

    (actor, sender)
}
