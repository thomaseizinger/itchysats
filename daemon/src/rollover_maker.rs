use crate::address_map::ActorName;
use crate::maker_cfd::RollOverCompleted;
use crate::maker_inc_connections::TakerMessage;
use crate::model::cfd::{OrderId, Role};
use crate::model::Identity;
use crate::oracle::GetAnnouncement;
use crate::projection::UpdateRollOverProposal;
use crate::setup_contract::RolloverParams;
use crate::tokio_ext::spawn_fallible;
use crate::wire::{MakerToTaker, RollOverMsg};
use crate::{
    log_error, maker_inc_connections, oracle, projection, schnorrsig, setup_contract, wire, Cfd,
    Stopping,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::{future, SinkExt};
use xtra::prelude::MessageChannel;
use xtra::{Context, Handler, KeepRunning};

pub struct AcceptRollOver;

pub struct RejectRollOver {
    pub order_id: OrderId,
}

pub struct Actor {
    send_to_taker_actor: Box<dyn MessageChannel<TakerMessage>>,
    cfd: Cfd,
    taker_id: Identity,
    n_payouts: usize,
    oracle_pk: schnorrsig::PublicKey,
    sent_from_taker: Option<UnboundedSender<RollOverMsg>>,
    maker_cfd_actor: Box<dyn MessageChannel<RollOverCompleted>>,
    oracle_actor: Box<dyn MessageChannel<GetAnnouncement>>,
    on_stopping: Vec<Box<dyn MessageChannel<Stopping<Self>>>>,
    projection_actor: xtra::Address<projection::Actor>,
}

#[async_trait::async_trait]
impl xtra::Actor for Actor {
    async fn stopping(&mut self, ctx: &mut Context<Self>) -> KeepRunning {
        let address = ctx.address().expect("acquired own actor address");

        for channel in self.on_stopping.iter() {
            let _ = channel
                .send(Stopping {
                    me: address.clone(),
                })
                .await;
        }

        KeepRunning::StopAll
    }
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        send_to_taker_actor: &(impl MessageChannel<TakerMessage> + 'static),
        cfd: Cfd,
        taker_id: Identity,
        oracle_pk: schnorrsig::PublicKey,
        maker_cfd_actor: &(impl MessageChannel<RollOverCompleted> + 'static),
        oracle_actor: &(impl MessageChannel<GetAnnouncement> + 'static),
        (on_stopping0, on_stopping1): (
            &(impl MessageChannel<Stopping<Self>> + 'static),
            &(impl MessageChannel<Stopping<Self>> + 'static),
        ),
        projection_actor: xtra::Address<projection::Actor>,
    ) -> Self {
        Self {
            send_to_taker_actor: send_to_taker_actor.clone_channel(),
            cfd,
            taker_id,
            n_payouts: 0,
            oracle_pk,
            sent_from_taker: None,
            maker_cfd_actor: maker_cfd_actor.clone_channel(),
            oracle_actor: oracle_actor.clone_channel(),
            on_stopping: vec![on_stopping0.clone_channel(), on_stopping1.clone_channel()],
            projection_actor,
        }
    }

    async fn handle_rollover_msg(&mut self, msg: RollOverMsg) -> Result<()> {
        let sender = self
            .sent_from_taker
            .as_mut()
            .context("cannot forward message to rollover task")?;
        sender.send(msg).await?;
        Ok(())
    }

    async fn handle_accept_rollover(
        &mut self,
        _msg: AcceptRollOver,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let order_id = self.cfd.order.id;

        let (sender, receiver) = mpsc::unbounded();

        self.sent_from_taker = Some(sender);

        tracing::debug!(%order_id, "Maker accepts a roll_over proposal" );

        let cfd = self.cfd.clone();

        let dlc = cfd.open_dlc().expect("CFD was in wrong state");

        let oracle_event_id = oracle::next_announcement_after(
            time::OffsetDateTime::now_utc() + cfd.order.settlement_interval,
        )?;

        let taker_id = self.taker_id;

        self.send_to_taker_actor
            .send(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::ConfirmRollOver {
                    order_id: cfd.order.id,
                    oracle_event_id,
                },
            })
            .await??;

        self.projection_actor
            .send(UpdateRollOverProposal {
                order: self.cfd.order.id,
                proposal: None,
            })
            .await?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", oracle_event_id))?;

        let rollover_fut = setup_contract::roll_over(
            self.send_to_taker_actor.sink().with(move |msg| {
                future::ok(maker_inc_connections::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::RollOverProtocol { order_id, msg },
                })
            }),
            receiver,
            (self.oracle_pk, announcement),
            RolloverParams::new(
                cfd.order.price,
                cfd.quantity_usd,
                cfd.order.leverage,
                cfd.refund_timelock_in_blocks(),
                cfd.order.fee_rate,
            ),
            Role::Maker,
            dlc,
            self.n_payouts,
        );

        let this = ctx.address().expect("acquired own actor address");
        spawn_fallible::<_, anyhow::Error>(async move {
            let _ = match rollover_fut.await {
                Ok(dlc) => {
                    this.send(RollOverCompleted::Success { order_id, dlc })
                        .await?
                }
                Err(error) => {
                    this.send(RollOverCompleted::Failed { order_id, error })
                        .await?
                }
            };
            Ok(())
        });

        Ok(())
    }

    async fn handle_reject_rollover(
        &mut self,
        msg: RejectRollOver,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        self.send_to_taker_actor
            .send(TakerMessage {
                taker_id: self.taker_id,
                msg: MakerToTaker::RejectRollOver(msg.order_id),
            })
            .await??;
        self.projection_actor
            .send(UpdateRollOverProposal {
                order: msg.order_id,
                proposal: None,
            })
            .await?;
        ctx.stop();
        Ok(())
    }

    async fn handle_rollover_completed(
        &mut self,
        msg: RollOverCompleted,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        self.maker_cfd_actor.send(msg).await?;
        ctx.stop();
        Ok(())
    }
}

#[async_trait]
impl Handler<AcceptRollOver> for Actor {
    async fn handle(&mut self, msg: AcceptRollOver, ctx: &mut Context<Self>) {
        log_error!(self.handle_accept_rollover(msg, ctx));
    }
}

#[async_trait]
impl Handler<RejectRollOver> for Actor {
    async fn handle(&mut self, msg: RejectRollOver, ctx: &mut Context<Self>) {
        log_error!(self.handle_reject_rollover(msg, ctx));
    }
}

#[async_trait]
impl Handler<RollOverCompleted> for Actor {
    async fn handle(&mut self, msg: RollOverCompleted, ctx: &mut Context<Self>) {
        log_error!(self.handle_rollover_completed(msg, ctx));
    }
}

#[async_trait]
impl Handler<RollOverMsg> for Actor {
    async fn handle(&mut self, msg: RollOverMsg, _ctx: &mut Context<Self>) {
        log_error!(self.handle_rollover_msg(msg));
    }
}

impl xtra::Message for AcceptRollOver {
    type Result = ();
}

impl xtra::Message for RejectRollOver {
    type Result = ();
}

impl ActorName for Actor {
    fn actor_name() -> String {
        "Maker rollover".to_string()
    }
}
