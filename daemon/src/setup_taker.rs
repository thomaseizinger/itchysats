use crate::model::cfd::{Cfd, CfdState, Completed, Dlc, OrderId, Role};
use crate::oracle::Announcement;
use crate::setup_contract::{self, SetupParams};
use crate::tokio_ext::spawn_fallible;
use crate::wire::{self, SetupMsg};
use crate::{connection, wallet};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::channel::mpsc::{self, UnboundedSender};
use futures::{future, SinkExt};
use maia::secp256k1_zkp::schnorrsig;
use xtra::prelude::*;
use xtra_productivity::xtra_productivity;

pub struct Actor {
    cfd: Cfd,
    n_payouts: usize,
    oracle_pk: schnorrsig::PublicKey,
    announcement: Announcement,
    build_party_params: Box<dyn MessageChannel<wallet::BuildPartyParams>>,
    sign: Box<dyn MessageChannel<wallet::Sign>>,
    maker: xtra::Address<connection::Actor>,
    on_accepted: Box<dyn MessageChannel<Started>>,
    on_completed: Box<dyn MessageChannel<Completed>>,
    setup_msg_sender: Option<UnboundedSender<SetupMsg>>,
}

impl Actor {
    pub fn new(
        (cfd, n_payouts): (Cfd, usize),
        (oracle_pk, announcement): (schnorrsig::PublicKey, Announcement),
        build_party_params: &(impl MessageChannel<wallet::BuildPartyParams> + 'static),
        sign: &(impl MessageChannel<wallet::Sign> + 'static),
        maker: xtra::Address<connection::Actor>,
        on_accepted: &(impl MessageChannel<Started> + 'static),
        on_completed: &(impl MessageChannel<Completed> + 'static),
    ) -> Self {
        Self {
            cfd,
            n_payouts,
            oracle_pk,
            announcement,
            build_party_params: build_party_params.clone_channel(),
            sign: sign.clone_channel(),
            maker,
            on_accepted: on_accepted.clone_channel(),
            on_completed: on_completed.clone_channel(),
            setup_msg_sender: None,
        }
    }
}

#[xtra_productivity]
impl Actor {
    fn handle(&mut self, _: Accepted, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let order_id = self.cfd.id;

        self.cfd.state = CfdState::contract_setup();

        tracing::info!(%order_id, "Order got accepted");

        // inform the `taker_cfd::Actor` about the start of contract
        // setup, so that the db and UI can be updated accordingly
        self.on_accepted.send(Started(order_id)).await?;

        let (sender, receiver) = mpsc::unbounded::<SetupMsg>();
        // store the writing end to forward messages from the maker to
        // the spawned contract setup task
        self.setup_msg_sender = Some(sender);

        let contract_future = setup_contract::new(
            xtra::message_channel::MessageChannel::sink(&self.maker)
                .with(move |msg| future::ok(wire::TakerToMaker::Protocol { order_id, msg })),
            receiver,
            (self.oracle_pk, self.announcement.clone()),
            SetupParams::new(
                self.cfd.margin()?,
                self.cfd.counterparty_margin()?,
                self.cfd.price,
                self.cfd.quantity_usd,
                self.cfd.leverage,
                self.cfd.refund_timelock_in_blocks(),
                self.cfd.fee_rate,
            ),
            self.build_party_params.clone_channel(),
            self.sign.clone_channel(),
            Role::Taker,
            self.n_payouts,
        );

        let this = ctx.address().expect("self to be alive");
        spawn_fallible::<_, anyhow::Error>(async move {
            let _ = match contract_future.await {
                Ok(dlc) => this.send(SetupSucceeded { order_id, dlc }).await?,
                Err(error) => this.send(SetupFailed { order_id, error }).await?,
            };

            Ok(())
        });

        Ok(())
    }

    fn handle(&mut self, msg: Rejected, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let order_id = self.cfd.id;
        tracing::info!(%order_id, "Order got rejected");

        if msg.is_invalid_order {
            tracing::error!(%order_id, "Rejection reason: Invalid order ID");
        }

        self.on_completed
            .send(Completed::Rejected { order_id })
            .await?;

        ctx.stop();

        Ok(())
    }

    fn handle(&mut self, msg: wire::SetupMsg, _ctx: &mut xtra::Context<Self>) -> Result<()> {
        let mut sender = self
            .setup_msg_sender
            .clone()
            .context("Cannot forward message to contract setup task")?;
        sender.send(msg).await?;

        Ok(())
    }

    fn handle(&mut self, msg: SetupSucceeded, ctx: &mut xtra::Context<Self>) -> Result<()> {
        self.on_completed
            .send(Completed::NewContract {
                order_id: msg.order_id,
                dlc: msg.dlc,
            })
            .await?;

        ctx.stop();

        Ok(())
    }

    fn handle(&mut self, msg: SetupFailed, ctx: &mut xtra::Context<Self>) -> Result<()> {
        self.on_completed
            .send(Completed::Failed {
                order_id: msg.order_id,
                error: msg.error,
            })
            .await?;

        ctx.stop();

        Ok(())
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let address = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let res = self
            .maker
            .send(connection::TakeOrder {
                order_id: self.cfd.id,
                quantity: self.cfd.quantity_usd,
                address,
            })
            .await;

        if let Err(e) = res {
            tracing::error!(%self.cfd.id, "Stopping setup_taker actor: {}", e);
            ctx.stop()
        }
    }
}

/// Message sent from the `connection::Actor` to the
/// `setup_taker::Actor` to notify that the order taken was accepted
/// by the maker.
pub struct Accepted;

/// Message sent from the `setup_taker::Actor` to the
/// `taker_cfd::Actor` to notify that the contract setup has started.
pub struct Started(pub OrderId);

/// Message sent from the `connection::Actor` to the
/// `setup_taker::Actor` to notify that the order taken was rejected
/// by the maker.
pub struct Rejected {
    /// Used to indicate whether the rejection stems from the order ID
    /// not being recognised by the maker.
    is_invalid_order: bool,
}

/// Message sent from the spawned task to `setup_taker::Actor` to
/// notify that the contract setup has finished successfully.
pub struct SetupSucceeded {
    order_id: OrderId,
    dlc: Dlc,
}

/// Message sent from the spawned task to `setup_taker::Actor` to
/// notify that the contract setup has failed.
pub struct SetupFailed {
    order_id: OrderId,
    error: anyhow::Error,
}

impl Rejected {
    /// Order was rejected by the maker for not specific reason.
    pub fn without_reason() -> Self {
        Rejected {
            is_invalid_order: false,
        }
    }

    /// Order was rejected by the maker because it did not recognise
    /// the order ID provided.
    pub fn invalid_order_id() -> Self {
        Rejected {
            is_invalid_order: true,
        }
    }
}

impl xtra::Message for Started {
    type Result = ();
}

impl xtra::Message for Completed {
    type Result = ();
}
