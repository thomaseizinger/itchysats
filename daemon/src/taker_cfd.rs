use crate::address_map::{AddressMap, Stopping};
use crate::cfd_actors::{self, append_cfd_state, insert_cfd_and_update_feed};
use crate::db::load_cfd_by_order_id;
use crate::model::cfd::{Cfd, CfdState, CfdStateCommon, Completed, Order, OrderId, Origin, Role};
use crate::model::{Price, Usd};
use crate::monitor::{self, MonitorParams};
use crate::{
    collab_settlement_taker, connection, log_error, oracle, projection, rollover_taker,
    setup_taker, wallet, Tasks,
};
use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use bdk::bitcoin::secp256k1::schnorrsig;
use xtra::prelude::*;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;

pub struct CurrentOrder(pub Option<Order>);

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

pub struct Actor<O, M, W> {
    db: sqlx::SqlitePool,
    wallet: Address<W>,
    oracle_pk: schnorrsig::PublicKey,
    projection_actor: Address<projection::Actor>,
    conn_actor: Address<connection::Actor>,
    monitor_actor: Address<M>,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    collab_settlement_actors: AddressMap<OrderId, collab_settlement_taker::Actor>,
    rollover_actors: AddressMap<OrderId, rollover_taker::Actor>,
    oracle_actor: Address<O>,
    n_payouts: usize,
    tasks: Tasks,
    current_order: Option<Order>,
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
    ) -> Self {
        Self {
            db,
            wallet,
            oracle_pk,
            projection_actor,
            conn_actor,
            monitor_actor,
            oracle_actor,
            n_payouts,
            setup_actors: AddressMap::default(),
            collab_settlement_actors: AddressMap::default(),
            rollover_actors: AddressMap::default(),
            tasks: Tasks::default(),
            current_order: None,
        }
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
        let cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

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
        let settlement_txid = settlement.tx.txid();

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        let dlc = cfd.dlc().context("No DLC in CFD")?;

        cfd.handle_proposal_signed(settlement)?;
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        self.monitor_actor
            .send(monitor::CollaborativeSettlement {
                order_id,
                tx: (settlement_txid, dlc.script_pubkey_for(Role::Taker)),
            })
            .await?;

        Ok(())
    }
}

impl<O, M, W> Actor<O, M, W> {
    async fn handle_new_order(&mut self, order: Option<Order>) -> Result<()> {
        tracing::trace!("new order {:?}", order);
        match order {
            Some(mut order) => {
                order.origin = Origin::Theirs;

                let mut conn = self.db.acquire().await?;

                if load_cfd_by_order_id(order.id, &mut conn).await.is_ok() {
                    bail!("Received order {} from maker, but already have a cfd in the database for that order. The maker did not properly remove the order.", order.id)
                }

                self.current_order = Some(order.clone());

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

    async fn append_cfd_state_rejected(&mut self, order_id: OrderId) -> Result<()> {
        tracing::debug!(%order_id, "Order rejected");

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::rejected();
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        Ok(())
    }

    async fn append_cfd_state_setup_failed(
        &mut self,
        order_id: OrderId,
        error: anyhow::Error,
    ) -> Result<()> {
        tracing::error!(%order_id, "Contract setup failed: {:#?}", error);

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::setup_failed(error.to_string());
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        Ok(())
    }

    /// Set the state of the CFD in the database to `ContractSetup`
    /// and update the corresponding projection.
    async fn handle_setup_started(&mut self, order_id: OrderId) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::contract_setup();
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

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

#[xtra_productivity]
impl<O, M, W> Actor<O, M, W>
where
    Self: xtra::Handler<Completed>,
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    W: xtra::Handler<wallet::BuildPartyParams> + xtra::Handler<wallet::Sign>,
{
    async fn handle_take_offer(&mut self, msg: TakeOffer, ctx: &mut Context<Self>) -> Result<()> {
        let TakeOffer { order_id, quantity } = msg;

        let disconnected = self
            .setup_actors
            .get_disconnected(order_id)
            .with_context(|| {
                format!(
                    "Contract setup for order {} is already in progress",
                    order_id
                )
            })?;

        let mut conn = self.db.acquire().await?;

        let current_order = self
            .current_order
            .clone()
            .context("No current order from maker")?;

        tracing::info!("Taking current order: {:?}", &current_order);

        let cfd = Cfd::new(current_order, quantity, CfdState::outgoing_order_request());

        insert_cfd_and_update_feed(&cfd, &mut conn, &self.projection_actor).await?;

        // Cleanup own order feed, after inserting the cfd.
        // Due to the 1:1 relationship between order and cfd we can never create another cfd for the
        // same order id.
        self.projection_actor.send(projection::Update(None)).await?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(cfd.oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", cfd.oracle_event_id))?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = setup_taker::Actor::new(
            (cfd, self.n_payouts),
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
    async fn handle_setup_completed(&mut self, msg: Completed) -> Result<()> {
        let (order_id, dlc) = match msg {
            Completed::NewContract { order_id, dlc } => (order_id, dlc),
            Completed::Rejected { order_id } => {
                self.append_cfd_state_rejected(order_id).await?;
                return Ok(());
            }
            Completed::Failed { order_id, error } => {
                self.append_cfd_state_setup_failed(order_id, error).await?;
                return Ok(());
            }
        };

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

        tracing::info!("Setup complete, publishing on chain now");

        cfd.state = CfdState::PendingOpen {
            common: CfdStateCommon::default(),
            dlc: dlc.clone(),
            attestation: None,
        };

        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

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
                params: MonitorParams::new(dlc.clone(), cfd.refund_timelock_in_blocks()),
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

#[xtra_productivity]
impl<O, M, W> Actor<O, M, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle_propose_rollover(
        &mut self,
        msg: ProposeRollOver,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        let ProposeRollOver { order_id } = msg;

        let disconnected = self
            .rollover_actors
            .get_disconnected(order_id)
            .with_context(|| format!("Rollover for order {} is already in progress", order_id))?;

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = rollover_taker::Actor::new(
            (cfd, self.n_payouts),
            self.oracle_pk,
            self.conn_actor.clone(),
            &self.oracle_actor,
            self.projection_actor.clone(),
            &this,
            (&this, &self.conn_actor),
        )
        .create(None)
        .run();

        disconnected.insert(addr);
        self.tasks.add(fut);

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M, W> Actor<O, M, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle_rollover_completed(&mut self, msg: rollover_taker::Completed) -> Result<()> {
        use rollover_taker::Completed::*;
        let (order_id, dlc) = match msg {
            UpdatedContract { order_id, dlc } => (order_id, dlc),
            Rejected { .. } => {
                return Ok(());
            }
            Failed { order_id, error } => {
                tracing::warn!(%order_id, "Rollover failed: {:#}", error);
                return Ok(());
            }
        };

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::Open {
            common: CfdStateCommon::default(),
            dlc: dlc.clone(),
            attestation: None,
            collaborative_close: None,
        };

        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        self.monitor_actor
            .send(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::new(dlc.clone(), cfd.refund_timelock_in_blocks()),
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

#[xtra_productivity(message_impl = false)]
impl<O, M, W> Actor<O, M, W> {
    async fn handle_rollover_actor_stopping(&mut self, msg: Stopping<rollover_taker::Actor>) {
        self.rollover_actors.gc(msg);
    }
}

#[xtra_productivity]
impl<O, M, W> Actor<O, M, W> {
    async fn handle_current_order(&mut self, msg: CurrentOrder) {
        log_error!(self.handle_new_order(msg.0));
    }
}

#[async_trait]
impl<O: 'static, M: 'static, W: 'static> Handler<Completed> for Actor<O, M, W>
where
    O: xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: Completed, _ctx: &mut Context<Self>) {
        log_error!(self.handle_setup_completed(msg))
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

impl<O: 'static, M: 'static, W: 'static> xtra::Actor for Actor<O, M, W> {}
