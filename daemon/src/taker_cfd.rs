use crate::address_map::AddressMap;
use crate::db::{append_events, insert_cfd, load_cfd};
use crate::model::cfd::{
    CfdAggregate, CfdEvent, Dlc, Order, OrderId, Origin, Role, RollOverProposal, SettlementKind,
    UpdateCfdProposal, UpdateCfdProposals,
};
use crate::model::{BitMexPriceEventId, Identity, Position, Price, Timestamp, Usd};
use crate::monitor::{self, MonitorParams};
use crate::projection::{
    try_into_update_rollover_proposal, UpdateRollOverProposal, UpdateSettlementProposal,
};
use crate::setup_contract::RolloverError;
use crate::tokio_ext::FutureExt;
use crate::wire::RollOverMsg;
use crate::{
    cfd_actors, collab_settlement_taker, connection, log_error, oracle, projection, setup_contract,
    setup_taker, wallet, wire, Tasks,
};
use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use bdk::bitcoin::secp256k1::schnorrsig;
use futures::channel::mpsc;
use futures::future::RemoteHandle;
use futures::{future, SinkExt};
use std::collections::HashMap;
use xtra::prelude::*;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;

pub struct TakeOffer {
    pub order_id: OrderId,
    pub quantity: Usd,
}

pub struct ProposeSettlement {
    pub order_id: OrderId,
    pub current_price: Price,
}

pub struct ProposeRollOver {
    pub order_id: OrderId,
}

pub struct Commit {
    pub order_id: OrderId,
}

pub struct CfdRollOverCompleted {
    pub order_id: OrderId,
    pub dlc: Result<Dlc, RolloverError>,
}

enum RollOverState {
    Active {
        sender: mpsc::UnboundedSender<RollOverMsg>,
        _task: RemoteHandle<()>,
    },
    None,
}

pub struct Actor<O, M, W> {
    db: sqlx::SqlitePool,
    wallet: Address<W>,
    oracle_pk: schnorrsig::PublicKey,
    projection_actor: Address<projection::Actor>,
    conn_actor: Address<connection::Actor>,
    monitor_actor: Address<M>,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    collab_settlement_actors: AddressMap<OrderId, collab_settlement_taker::Actor>,
    roll_over_state: RollOverState,
    oracle_actor: Address<O>,
    current_pending_proposals: UpdateCfdProposals,
    n_payouts: usize,
    current_maker_order: Option<Order>,
    maker_identity: Identity,
    tasks: Tasks,
}

impl<O, M, W> Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlx::SqlitePool,
        wallet: Address<W>,
        oracle_pk: schnorrsig::PublicKey,
        projection_actor: Address<projection::Actor>,
        conn_actor: Address<connection::Actor>,
        monitor_actor: Address<M>,
        oracle_actor: Address<O>,
        n_payouts: usize,
        maker_identity: Identity,
    ) -> Self {
        Self {
            db,
            wallet,
            oracle_pk,
            projection_actor,
            conn_actor,
            monitor_actor,
            roll_over_state: RollOverState::None,
            oracle_actor,
            current_pending_proposals: HashMap::new(),
            n_payouts,
            setup_actors: AddressMap::default(),
            collab_settlement_actors: AddressMap::default(),
            current_maker_order: None,
            maker_identity,
            tasks: Tasks::default(),
        }
    }
}

impl<O, M, W> Actor<O, M, W> {
    /// Removes a proposal and updates the update cfd proposals' feed
    async fn remove_pending_proposal(&mut self, order_id: &OrderId) -> Result<()> {
        let removed_proposal = self.current_pending_proposals.remove(order_id);

        if let Some(removed_proposal) = removed_proposal {
            match removed_proposal {
                UpdateCfdProposal::Settlement { .. } => {
                    self.projection_actor
                        .send(UpdateSettlementProposal {
                            order: *order_id,
                            proposal: None,
                        })
                        .await?
                }
                UpdateCfdProposal::RollOverProposal { .. } => {
                    self.projection_actor
                        .send(UpdateRollOverProposal {
                            order: *order_id,
                            proposal: None,
                        })
                        .await?
                }
            }
        } else {
            anyhow::bail!("Could not find proposal with order id: {}", &order_id);
        }
        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, W> Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
    M: xtra::Handler<monitor::CollaborativeSettlement>,
{
    async fn handle_commit(&mut self, msg: Commit) -> Result<()> {
        let Commit { order_id } = msg;

        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_commit(order_id, &mut conn, &self.wallet, &self.projection_actor)
            .await?;
        Ok(())
    }

    async fn handle_propose_roll_over(&mut self, msg: ProposeRollOver) -> Result<()> {
        let ProposeRollOver { order_id } = msg;

        if self.current_pending_proposals.contains_key(&order_id) {
            anyhow::bail!("An update for order id {} is already in progress", order_id)
        }

        let proposal = RollOverProposal {
            order_id,
            timestamp: Timestamp::now(),
        };

        let new_proposal = UpdateCfdProposal::RollOverProposal {
            proposal: proposal.clone(),
            direction: SettlementKind::Outgoing,
        };

        self.current_pending_proposals
            .insert(proposal.order_id, new_proposal.clone());
        self.projection_actor
            .send(try_into_update_rollover_proposal(new_proposal)?)
            .await?;

        self.conn_actor
            .send(wire::TakerToMaker::ProposeRollOver {
                order_id: proposal.order_id,
                timestamp: proposal.timestamp,
            })
            .await?;
        Ok(())
    }

    async fn handle_propose_settlement(
        &mut self,
        msg: ProposeSettlement,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let ProposeSettlement {
            order_id,
            current_price,
        } = msg;

        let disconnected = self
            .collab_settlement_actors
            .get_disconnected(order_id)
            .with_context(|| format!("Settlement for order {} is already in progress", order_id))?;

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd(order_id, &mut conn).await?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = collab_settlement_taker::Actor::new(
            cfd,
            self.projection_actor.clone(),
            this,
            current_price,
            self.conn_actor.clone(),
            self.n_payouts,
        )?
        .create(None)
        .run();

        disconnected.insert(addr);
        self.tasks.add(fut);

        Ok(())
    }

    async fn handle_settlement_completed(
        &mut self,
        msg: collab_settlement_taker::Completed,
    ) -> Result<()> {
        let (order_id, settlement) = match msg {
            collab_settlement_taker::Completed::Confirmed {
                order_id,
                settlement,
            } => (order_id, settlement),
            collab_settlement_taker::Completed::Rejected { .. } => {
                return Ok(());
            }
            collab_settlement_taker::Completed::Failed { order_id, error } => {
                tracing::warn!(%order_id, "Collaborative settlement failed: {:#}", error);
                return Ok(());
            }
        };
        let _settlement_txid = settlement.tx.txid();

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd(order_id, &mut conn).await?;

        let event = cfd.settle_collaboratively_taker(settlement);
        append_events(event, &mut conn).await?;

        // TODO: We used to update the UI here.
        // TODO: CollaborativeSettlement needs to include payout address for monitoring.

        // self.monitor_actor
        //     .send(monitor::CollaborativeSettlement {
        //         order_id,
        //         tx: (settlement_txid, dlc.script_pubkey_for(Role::Taker)),
        //     })
        //     .await?;

        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>,
{
    async fn handle_roll_over_rejected(&mut self, order_id: OrderId) -> Result<()> {
        tracing::info!(%order_id, "Roll over proposal got rejected");

        self.remove_pending_proposal(&order_id)
            .await
            .context("rejected settlement")?;

        Ok(())
    }

    async fn handle_inc_roll_over_msg(&mut self, msg: RollOverMsg) -> Result<()> {
        match &mut self.roll_over_state {
            RollOverState::Active { sender, .. } => {
                sender.send(msg).await?;
            }
            RollOverState::None => {
                anyhow::bail!("Received message without an active roll_over setup")
            }
        }

        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W> {
    async fn handle_new_order(&mut self, order: Option<Order>) -> Result<()> {
        tracing::trace!("new order {:?}", order);
        match order {
            Some(mut order) => {
                order.origin = Origin::Theirs; // TODO: We should no longer need this, only the maker can send orders.

                // let mut conn = self.db.acquire().await?;

                // TODO
                // if load_cfd_by_order_id(order.id, &mut conn).await.is_ok() {
                //     bail!("Received order {} from maker, but already have a cfd in the database
                // for that order. The maker did not properly remove the order.", order.id)
                // }

                self.current_maker_order = Some(order.clone());

                self.projection_actor
                    .send(projection::Update(Some(order)))
                    .await?;
            }
            None => {
                self.projection_actor.send(projection::Update(None)).await?;
            }
        }
        Ok(())
    }

    /// Set the state of the CFD in the database to `ContractSetup`
    /// and update the corresponding projection.
    async fn handle_setup_started(&mut self, _order_id: OrderId) -> Result<()> {
        // let mut conn = self.db.acquire().await?;

        // TODO: We used to update the UI here.

        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_monitoring_event(&mut self, event: monitor::Event) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_monitoring_event(event, &mut conn, &self.wallet, &self.projection_actor)
            .await?;
        Ok(())
    }

    async fn handle_oracle_attestation(&mut self, attestation: oracle::Attestation) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_oracle_attestation(
            attestation,
            &mut conn,
            &self.wallet,
            &self.projection_actor,
        )
        .await?;
        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W>
where
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    W: xtra::Handler<wallet::BuildPartyParams>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::TryBroadcastTransaction>,
    M: xtra::Handler<monitor::StartMonitoring>,
{
    async fn handle_take_offer(
        &mut self,
        order_id: OrderId,
        quantity: Usd,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        let disconnected = self
            .setup_actors
            .get_disconnected(order_id)
            .with_context(|| {
                format!(
                    "Contract setup for order {} is already in progress",
                    order_id
                )
            })?;

        let current_order = self
            .current_maker_order
            .as_ref()
            .context("No order present that can be taken")?
            .clone();

        // We create the cfd here without any events yet, only static data
        // Once the contract setup completes (rejected / accepted / failed) the first event will be
        // recorded
        let cfd = CfdAggregate::new(
            current_order.id,
            Position::Long,
            current_order.price,
            current_order.leverage,
            current_order.settlement_interval,
            quantity,
            self.maker_identity,
            Role::Taker,
        );
        let mut conn = self.db.acquire().await?;
        insert_cfd(&cfd, &mut conn).await?;

        tracing::info!("Taking current order: {:?}", &current_order);

        // Cleanup own order feed, after inserting the cfd.
        // Due to the 1:1 relationship between order and cfd we can never create another cfd for the
        // same order id.
        self.projection_actor.send(projection::Update(None)).await?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(current_order.oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", current_order.oracle_event_id))?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = setup_taker::Actor::new(
            (cfd, quantity, self.n_payouts),
            (self.oracle_pk, announcement),
            &self.wallet,
            &self.wallet,
            self.conn_actor.clone(),
            &this,
            &this,
        )
        .create(None)
        .run();

        disconnected.insert(addr);

        self.tasks.add(fut);

        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W>
where
    O: xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_setup_completed(&mut self, msg: setup_taker::Completed) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let order_id = msg.order_id();

        let cfd = load_cfd(order_id, &mut conn).await?;

        let event = cfd.setup_contract(msg)?;
        append_events(vec![event.clone()], &mut conn).await?;

        tracing::info!("Setup complete, publishing on chain now");

        // TODO: We used to update the UI here

        let dlc = match event.event {
            CfdEvent::ContractSetupCompleted { dlc } => dlc,
            CfdEvent::ContractSetupFailed { .. } => {
                unimplemented!("We don't deal with contract setup failed scenarios yet")
            }
            _ => bail!("Unexpected event {:?}", event.event),
        };

        let txid = self
            .wallet
            .send(wallet::TryBroadcastTransaction {
                tx: dlc.lock.0.clone(),
            })
            .await??;

        tracing::info!("Lock transaction published with txid {}", txid);

        self.monitor_actor
            .send(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::new(dlc.clone()),
            })
            .await?;

        self.oracle_actor
            .send(oracle::MonitorAttestation {
                event_id: dlc.settlement_event_id,
            })
            .await?;

        Ok(())
    }
}

impl<O: 'static, M: 'static, W: 'static> Actor<O, M, W>
where
    Self: xtra::Handler<CfdRollOverCompleted>,
    O: xtra::Handler<oracle::GetAnnouncement>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>,
{
    async fn handle_roll_over_accepted(
        &mut self,
        order_id: OrderId,
        oracle_event_id: BitMexPriceEventId,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::info!(%order_id, "Roll; over request got accepted");

        let (sender, receiver) = mpsc::unbounded();

        if let RollOverState::Active { .. } = self.roll_over_state {
            anyhow::bail!("Already rolling over a contract!")
        }

        let mut conn = self.db.acquire().await?;

        let cfd = load_cfd(order_id, &mut conn).await?;
        let (rollover_params, dlc, _) = cfd.start_rollover()?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", oracle_event_id))?;

        let contract_future = setup_contract::roll_over(
            xtra::message_channel::MessageChannel::sink(&self.conn_actor)
                .with(|msg| future::ok(wire::TakerToMaker::RollOverProtocol(msg))),
            receiver,
            (self.oracle_pk, announcement),
            rollover_params,
            Role::Taker,
            dlc,
            self.n_payouts,
        );

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        let task = async move {
            let dlc = contract_future.await;

            this.send(CfdRollOverCompleted { order_id, dlc })
                .await
                .expect("always connected to ourselves")
        }
        .spawn_with_handle();

        self.roll_over_state = RollOverState::Active {
            sender,
            _task: task,
        };

        self.remove_pending_proposal(&order_id)
            .await
            .context("Could not remove accepted roll over")?;
        Ok(())
    }
}

impl<O: 'static, M: 'static, W: 'static> Actor<O, M, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle_roll_over_completed(
        &mut self,
        order_id: OrderId,
        dlc: Result<Dlc, RolloverError>,
    ) -> Result<()> {
        self.roll_over_state = RollOverState::None;

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd(order_id, &mut conn).await?;

        let event = cfd.roll_over(dlc)?;
        append_events(vec![event.clone()], &mut conn).await?;

        let dlc = match event.event {
            CfdEvent::RolloverCompleted { dlc } => dlc,
            CfdEvent::RolloverFailed { .. } => {
                unimplemented!("We are not dealing with failed rollover atm")
            }
            _ => bail!("Unexpected event {:?}", event.event),
        };

        // TODO: We used to update the UI here

        self.monitor_actor
            .send(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::new(dlc.clone()),
            })
            .await?;

        self.oracle_actor
            .send(oracle::MonitorAttestation {
                event_id: dlc.settlement_event_id,
            })
            .await?;

        Ok(())
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<TakeOffer> for Actor<O, M, W>
where
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    W: xtra::Handler<wallet::BuildPartyParams>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::TryBroadcastTransaction>,
    M: xtra::Handler<monitor::StartMonitoring>,
{
    async fn handle(&mut self, msg: TakeOffer, ctx: &mut Context<Self>) -> Result<()> {
        self.handle_take_offer(msg.order_id, msg.quantity, ctx)
            .await
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<wire::MakerToTaker> for Actor<O, M, W>
where
    Self: xtra::Handler<CfdRollOverCompleted>,
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>,
{
    async fn handle(&mut self, msg: wire::MakerToTaker, ctx: &mut Context<Self>) {
        match msg {
            wire::MakerToTaker::CurrentOrder(current_order) => {
                log_error!(self.handle_new_order(current_order))
            }
            wire::MakerToTaker::ConfirmRollOver {
                order_id,
                oracle_event_id,
            } => {
                log_error!(self.handle_roll_over_accepted(order_id, oracle_event_id, ctx))
            }
            wire::MakerToTaker::RejectRollOver(order_id) => {
                log_error!(self.handle_roll_over_rejected(order_id))
            }
            wire::MakerToTaker::RollOverProtocol(roll_over_msg) => {
                log_error!(self.handle_inc_roll_over_msg(roll_over_msg))
            }
            wire::MakerToTaker::Heartbeat => {
                unreachable!("Heartbeats should be handled somewhere else")
            }
            wire::MakerToTaker::ConfirmOrder(_)
            | wire::MakerToTaker::RejectOrder(_)
            | wire::MakerToTaker::Protocol { .. }
            | wire::MakerToTaker::InvalidOrderId(_) => {
                unreachable!("These messages should be sent to the `setup_taker::Actor`")
            }
            wire::MakerToTaker::Settlement { .. } => {
                unreachable!("These messages should be sent to the `collab_settlement::Actor`")
            }
        }
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<setup_taker::Completed> for Actor<O, M, W>
where
    O: xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: setup_taker::Completed, _ctx: &mut Context<Self>) {
        log_error!(self.handle_setup_completed(msg))
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<CfdRollOverCompleted> for Actor<O, M, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle(&mut self, msg: CfdRollOverCompleted, _ctx: &mut Context<Self>) {
        log_error!(self.handle_roll_over_completed(msg.order_id, msg.dlc));
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<monitor::Event> for Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: monitor::Event, _ctx: &mut Context<Self>) {
        log_error!(self.handle_monitoring_event(msg))
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<oracle::Attestation> for Actor<O, M, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: oracle::Attestation, _ctx: &mut Context<Self>) {
        log_error!(self.handle_oracle_attestation(msg))
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<setup_taker::Started> for Actor<O, M, W> {
    async fn handle(&mut self, msg: setup_taker::Started, _ctx: &mut Context<Self>) {
        log_error!(self.handle_setup_started(msg.0))
    }
}

impl Message for TakeOffer {
    type Result = Result<()>;
}

impl Message for CfdRollOverCompleted {
    type Result = ();
}

impl<O: 'static, M: 'static, W: 'static> xtra::Actor for Actor<O, M, W> {}
