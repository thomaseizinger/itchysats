use crate::address_map::{AddressMap, Stopping};
use crate::cfd_actors::{self, append_cfd_state, insert_cfd_and_update_feed};
use crate::db::{insert_order, load_cfd_by_order_id, load_order_by_id};
use crate::model::cfd::{
    Cfd, CfdState, CfdStateCommon, Dlc, Order, OrderId, Origin, RollOverProposal, SettlementKind,
    SettlementProposal, UpdateCfdProposal,
};
use crate::model::{Identity, Price, Timestamp, Usd};
use crate::monitor::MonitorParams;
use crate::projection::{try_into_update_rollover_proposal, Update};
use crate::wire::TakerToMaker;
use crate::{
    collab_settlement_maker, log_error, maker_inc_connections, monitor, oracle, projection,
    rollover_maker, setup_maker, wallet, wire, Tasks,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use bdk::bitcoin::secp256k1::schnorrsig;
use sqlx::pool::PoolConnection;
use sqlx::Sqlite;
use std::collections::HashSet;
use time::Duration;
use xtra::prelude::*;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;

pub struct AcceptOrder {
    pub order_id: OrderId,
}
pub struct RejectOrder {
    pub order_id: OrderId,
}
pub struct AcceptSettlement {
    pub order_id: OrderId,
}
pub struct RejectSettlement {
    pub order_id: OrderId,
}
pub struct AcceptRollOver {
    pub order_id: OrderId,
}
pub struct RejectRollOver {
    pub order_id: OrderId,
}
pub struct Commit {
    pub order_id: OrderId,
}
pub struct NewOrder {
    pub price: Price,
    pub min_quantity: Usd,
    pub max_quantity: Usd,
    pub fee_rate: u32,
}

pub struct TakerConnected {
    pub id: Identity,
}

pub struct TakerDisconnected {
    pub id: Identity,
}

#[allow(clippy::large_enum_variant)]
pub enum RollOverCompleted {
    Success {
        order_id: OrderId,
        dlc: Dlc,
    },
    Rejected {
        order_id: OrderId,
    },
    Failed {
        order_id: OrderId,
        error: anyhow::Error,
    },
}

pub struct FromTaker {
    pub taker_id: Identity,
    pub msg: wire::TakerToMaker,
}

pub struct Actor<O, M, T, W> {
    db: sqlx::SqlitePool,
    wallet: Address<W>,
    settlement_interval: Duration,
    oracle_pk: schnorrsig::PublicKey,
    projection_actor: Address<projection::Actor>,
    rollover_actors: AddressMap<OrderId, rollover_maker::Actor>,
    takers: Address<T>,
    current_order_id: Option<OrderId>,
    monitor_actor: Address<M>,
    setup_actors: AddressMap<OrderId, setup_maker::Actor>,
    settlement_actors: AddressMap<OrderId, collab_settlement_maker::Actor>,
    oracle_actor: Address<O>,
    connected_takers: HashSet<Identity>,
    n_payouts: usize,
    tasks: Tasks,
}

impl<O, M, T, W> Actor<O, M, T, W> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlx::SqlitePool,
        wallet: Address<W>,
        settlement_interval: Duration,
        oracle_pk: schnorrsig::PublicKey,
        projection_actor: Address<projection::Actor>,
        takers: Address<T>,
        monitor_actor: Address<M>,
        oracle_actor: Address<O>,
        n_payouts: usize,
    ) -> Self {
        Self {
            db,
            wallet,
            settlement_interval,
            oracle_pk,
            projection_actor,
            rollover_actors: AddressMap::default(),
            takers,
            current_order_id: None,
            monitor_actor,
            setup_actors: AddressMap::default(),
            oracle_actor,
            n_payouts,
            connected_takers: HashSet::new(),
            settlement_actors: AddressMap::default(),
            tasks: Tasks::default(),
        }
    }

    async fn update_connected_takers(&mut self) -> Result<()> {
        self.projection_actor
            .send(Update(
                self.connected_takers
                    .clone()
                    .into_iter()
                    .collect::<Vec<Identity>>(),
            ))
            .await?;
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

    async fn append_cfd_state_rejected(&mut self, order_id: OrderId) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::rejected();
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        Ok(())
    }
}

impl<O, M, T, W> Actor<O, M, T, W>
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

impl<O, M, T, W> Actor<O, M, T, W>
where
    T: xtra::Handler<maker_inc_connections::TakerMessage>,
{
    async fn handle_taker_connected(&mut self, taker_id: Identity) -> Result<()> {
        let mut conn = self.db.acquire().await?;

        let current_order = match self.current_order_id {
            Some(current_order_id) => Some(load_order_by_id(current_order_id, &mut conn).await?),
            None => None,
        };

        // Need to use `do_send_async` here because we are being invoked from the
        // `maker_inc_connections::Actor`. Using `send` would result in a deadlock.
        #[allow(clippy::disallowed_method)]
        self.takers
            .do_send_async(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::CurrentOrder(current_order),
            })
            .await?;

        if !self.connected_takers.insert(taker_id) {
            tracing::warn!("Taker already connected: {:?}", &taker_id);
        }
        self.update_connected_takers().await?;
        Ok(())
    }

    async fn handle_taker_disconnected(&mut self, taker_id: Identity) -> Result<()> {
        if !self.connected_takers.remove(&taker_id) {
            tracing::warn!("Removed unknown taker: {:?}", &taker_id);
        }
        self.update_connected_takers().await?;
        Ok(())
    }

    async fn reject_order(
        &mut self,
        taker_id: Identity,
        mut cfd: Cfd,
        mut conn: PoolConnection<Sqlite>,
    ) -> Result<()> {
        cfd.state = CfdState::rejected();

        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        self.takers
            .send(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::RejectOrder(cfd.order.id),
            })
            .await??;

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_accept_rollover(&mut self, msg: AcceptRollOver) -> Result<()> {
        if self
            .rollover_actors
            .send(&msg.order_id, rollover_maker::AcceptRollOver)
            .await
            .is_err()
        {
            tracing::warn!(%msg.order_id, "No active rollover");
        }

        Ok(())
    }
}

impl<O, M, T, W> Actor<O, M, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    T: xtra::Handler<maker_inc_connections::TakerMessage>
        + xtra::Handler<Stopping<rollover_maker::Actor>>,
    W: 'static,
    Self: xtra::Handler<Stopping<rollover_maker::Actor>>,
{
    async fn handle_propose_roll_over(
        &mut self,
        proposal: RollOverProposal,
        taker_id: Identity,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::info!(
            "Received proposal from the taker {}: {:?} to roll over order {}",
            taker_id,
            proposal,
            proposal.order_id
        );

        // check if CFD is in open state, otherwise we should not proceed
        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd_by_order_id(proposal.order_id, &mut conn).await?;
        match cfd {
            Cfd {
                state: CfdState::Open { .. },
                ..
            } => (),
            _ => {
                anyhow::bail!("Order is in invalid state. Cannot propose roll over.")
            }
        };

        let this = ctx.address().expect("acquired own address");

        let (rollover_actor_addr, _rollover_actor_future) = rollover_maker::Actor::new(
            &self.takers,
            cfd,
            taker_id,
            self.oracle_pk,
            &this,
            &self.oracle_actor,
            (&self.takers, &this),
            self.projection_actor.clone(),
        )
        .create(None)
        .run();

        self.rollover_actors
            .insert(proposal.order_id, rollover_actor_addr);

        let new_proposal = UpdateCfdProposal::RollOverProposal {
            proposal: proposal.clone(),
            direction: SettlementKind::Incoming,
        };

        self.projection_actor
            .send(try_into_update_rollover_proposal(new_proposal)?)
            .await?;

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_rollover_actor_stopping(&mut self, msg: Stopping<rollover_maker::Actor>) {
        self.rollover_actors.gc(msg);
    }
}

impl<O, M, T, W> Actor<O, M, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    T: xtra::Handler<maker_inc_connections::ConfirmOrder>
        + xtra::Handler<maker_inc_connections::TakerMessage>
        + xtra::Handler<maker_inc_connections::BroadcastOrder>
        + xtra::Handler<Stopping<setup_maker::Actor>>,
    W: xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>
        + xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_take_order(
        &mut self,
        taker_id: Identity,
        order_id: OrderId,
        quantity: Usd,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::debug!(%taker_id, %quantity, %order_id, "Taker wants to take an order");

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

        // 1. Validate if order is still valid
        let current_order = match self.current_order_id {
            Some(current_order_id) if current_order_id == order_id => {
                load_order_by_id(current_order_id, &mut conn).await?
            }
            _ => {
                // An outdated order on the taker side does not require any state change on the
                // maker. notifying the taker with a specific message should be sufficient.
                // Since this is a scenario that we should rarely see we log
                // a warning to be sure we don't trigger this code path frequently.
                tracing::warn!("Taker tried to take order with outdated id {}", order_id);

                self.takers
                    .send(maker_inc_connections::TakerMessage {
                        taker_id,
                        msg: wire::MakerToTaker::InvalidOrderId(order_id),
                    })
                    .await??;

                return Ok(());
            }
        };

        // 2. Remove current order
        // The order is removed before we update the state, because the maker might react on the
        // state change. Once we know that we go for either an accept/reject scenario we
        // have to remove the current order.
        self.current_order_id = None;

        // Need to use `do_send_async` here because invoking the
        // corresponding handler can result in a deadlock with another
        // invocation in `maker_inc_connections.rs`
        #[allow(clippy::disallowed_method)]
        self.takers
            .do_send_async(maker_inc_connections::BroadcastOrder(None))
            .await?;

        self.projection_actor.send(projection::Update(None)).await?;

        // 3. Insert CFD in DB
        let cfd = Cfd::new(
            current_order.clone(),
            quantity,
            CfdState::IncomingOrderRequest {
                common: CfdStateCommon {
                    transition_timestamp: Timestamp::now(),
                },
                taker_id,
            },
            taker_id,
        );
        insert_cfd_and_update_feed(&cfd, &mut conn, &self.projection_actor).await?;

        // 4. Try to get the oracle announcement, if that fails we should exit prior to changing any
        // state
        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(cfd.order.oracle_event_id))
            .await??;

        // 5. Start up contract setup actor
        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = setup_maker::Actor::new(
            (cfd.order, cfd.quantity_usd, self.n_payouts),
            (self.oracle_pk, announcement),
            &self.wallet,
            &self.wallet,
            (&self.takers, &self.takers, taker_id),
            &this,
            (&self.takers, &this),
        )
        .create(None)
        .run();

        disconnected.insert(addr);

        self.tasks.add(fut);

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_accept_order(&mut self, msg: AcceptOrder) -> Result<()> {
        let AcceptOrder { order_id } = msg;

        tracing::debug!(%order_id, "Maker accepts order");

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

        self.setup_actors
            .send(&order_id, setup_maker::Accepted)
            .await
            .with_context(|| format!("No active contract setup for order {}", order_id))?;

        cfd.state = CfdState::contract_setup();
        append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_setup_actor_stopping(&mut self, message: Stopping<setup_maker::Actor>) {
        self.setup_actors.gc(message);
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_settlement_actor_stopping(
        &mut self,
        message: Stopping<collab_settlement_maker::Actor>,
    ) {
        self.settlement_actors.gc(message);
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W>
where
    T: xtra::Handler<maker_inc_connections::TakerMessage>,
{
    async fn handle_reject_order(&mut self, msg: RejectOrder) -> Result<()> {
        let RejectOrder { order_id } = msg;

        tracing::debug!(%order_id, "Maker rejects order");

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

        let taker_id = match cfd {
            Cfd {
                state: CfdState::IncomingOrderRequest { taker_id, .. },
                ..
            } => taker_id,
            _ => {
                anyhow::bail!("Order is in invalid state. Ignoring trying to reject it.")
            }
        };

        self.reject_order(taker_id, cfd, conn).await?;

        Ok(())
    }

    async fn handle_accept_settlement(&mut self, msg: AcceptSettlement) -> Result<()> {
        let AcceptSettlement { order_id } = msg;

        self.settlement_actors
            .send(&order_id, collab_settlement_maker::Accepted)
            .await
            .with_context(|| format!("No settlement in progress for order {}", order_id))?;

        Ok(())
    }

    async fn handle_reject_settlement(&mut self, msg: RejectSettlement) -> Result<()> {
        let RejectSettlement { order_id } = msg;

        self.settlement_actors
            .send(&order_id, collab_settlement_maker::Rejected)
            .await
            .with_context(|| format!("No settlement in progress for order {}", order_id))?;

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W>
where
    M: xtra::Handler<monitor::CollaborativeSettlement>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_settlement_completed(&mut self, msg: collab_settlement_maker::Completed) {
        log_error!(async {
            use collab_settlement_maker::Completed::*;
            let (order_id, settlement, script_pubkey) = match msg {
                Confirmed {
                    order_id,
                    settlement,
                    script_pubkey,
                } => (order_id, settlement, script_pubkey),
                Rejected { .. } => {
                    return Ok(());
                }
                Failed { order_id, error } => {
                    tracing::warn!(%order_id, "Collaborative settlement failed: {:#}", error);
                    return Ok(());
                }
            };

            let mut conn = self.db.acquire().await?;
            let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

            let tx = settlement.tx.clone();
            cfd.handle_proposal_signed(settlement)
                .context("Failed to update state with collaborative settlement")?;

            append_cfd_state(&cfd, &mut conn, &self.projection_actor).await?;

            let txid = self
                .wallet
                .send(wallet::TryBroadcastTransaction { tx })
                .await?
                .context("Broadcasting close transaction")?;
            tracing::info!(%order_id, "Close transaction published with txid {}", txid);

            self.monitor_actor
                .send(monitor::CollaborativeSettlement {
                    order_id,
                    tx: (txid, script_pubkey),
                })
                .await?;

            anyhow::Ok(())
        });
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W> {
    async fn handle_reject_rollover(&mut self, msg: RejectRollOver) -> Result<()> {
        tracing::info!(%msg.order_id, "Maker rejects a roll_over proposal" );

        if self
            .rollover_actors
            .send(
                &msg.order_id,
                rollover_maker::RejectRollOver {
                    order_id: msg.order_id,
                },
            )
            .await
            .is_err()
        {
            tracing::warn!(%msg.order_id, "No active rollover");
        }

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_commit(&mut self, msg: Commit) -> Result<()> {
        let Commit { order_id } = msg;

        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_commit(order_id, &mut conn, &self.wallet, &self.projection_actor)
            .await?;

        Ok(())
    }
}

impl<O, M, T, W> Actor<O, M, T, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle_roll_over_completed(&mut self, msg: RollOverCompleted) -> Result<()> {
        match msg {
            RollOverCompleted::Success { order_id, dlc } => {
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
            }
            RollOverCompleted::Rejected { order_id } => {
                self.projection_actor
                    .send(projection::UpdateRollOverProposal {
                        order: order_id,
                        proposal: None,
                    })
                    .await?;
                tracing::info!(%order_id, "Rollover rejected");
            }
            RollOverCompleted::Failed { order_id, .. } => {
                self.projection_actor
                    .send(projection::UpdateRollOverProposal {
                        order: order_id,
                        proposal: None,
                    })
                    .await?;
                tracing::info!(%order_id, "Rollover failed");
            }
        };

        Ok(())
    }
}

impl<O, M, T, W> Actor<O, M, T, W>
where
    O: xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring> + xtra::Handler<monitor::CollaborativeSettlement>,
    T: xtra::Handler<maker_inc_connections::settlement::Response>
        + xtra::Handler<Stopping<collab_settlement_maker::Actor>>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_propose_settlement(
        &mut self,
        taker_id: Identity,
        proposal: SettlementProposal,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let disconnected = self
            .settlement_actors
            .get_disconnected(proposal.order_id)
            .with_context(|| {
                format!(
                    "Settlement for order {} is already in progress",
                    proposal.order_id
                )
            })?;

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd_by_order_id(proposal.order_id, &mut conn).await?;

        let this = ctx.address().expect("self to be alive");
        let (addr, task) = collab_settlement_maker::Actor::new(
            cfd,
            proposal,
            self.projection_actor.clone(),
            &ctx.address().expect("we are alive"),
            taker_id,
            &self.takers,
            (&self.takers, &this),
        )
        .create(None)
        .run();

        self.tasks.add(task);
        disconnected.insert(addr);

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W>
where
    T: xtra::Handler<maker_inc_connections::BroadcastOrder>,
{
    async fn handle_new_order(&mut self, msg: NewOrder) -> Result<()> {
        let NewOrder {
            price,
            min_quantity,
            max_quantity,
            fee_rate,
        } = msg;

        let oracle_event_id = oracle::next_announcement_after(
            time::OffsetDateTime::now_utc() + self.settlement_interval,
        )?;

        let order = Order::new(
            price,
            min_quantity,
            max_quantity,
            Origin::Ours,
            oracle_event_id,
            self.settlement_interval,
            fee_rate,
        )?;

        // 1. Save to DB
        let mut conn = self.db.acquire().await?;
        insert_order(&order, &mut conn).await?;

        // 2. Update actor state to current order
        self.current_order_id.replace(order.id);

        // 3. Notify UI via feed
        self.projection_actor
            .send(projection::Update(Some(order.clone())))
            .await?;

        // 4. Inform connected takers
        self.takers
            .send(maker_inc_connections::BroadcastOrder(Some(order)))
            .await?;

        Ok(())
    }
}

#[xtra_productivity]
impl<O, M, T, W> Actor<O, M, T, W>
where
    O: xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring>,
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_setup_completed(&mut self, msg: setup_maker::Completed) {
        log_error!(async {
            use setup_maker::Completed::*;
            let (order_id, dlc) = match msg {
                NewContract { order_id, dlc } => (order_id, dlc),
                Failed { order_id, error } => {
                    self.append_cfd_state_setup_failed(order_id, error).await?;
                    return anyhow::Ok(());
                }
                Rejected(order_id) => {
                    self.append_cfd_state_rejected(order_id).await?;
                    return anyhow::Ok(());
                }
            };

            let mut conn = self.db.acquire().await?;
            let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

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
        });
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<TakerConnected> for Actor<O, M, T, W>
where
    T: xtra::Handler<maker_inc_connections::TakerMessage>,
{
    async fn handle(&mut self, msg: TakerConnected, _ctx: &mut Context<Self>) {
        log_error!(self.handle_taker_connected(msg.id));
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<TakerDisconnected>
    for Actor<O, M, T, W>
where
    T: xtra::Handler<maker_inc_connections::TakerMessage>,
{
    async fn handle(&mut self, msg: TakerDisconnected, _ctx: &mut Context<Self>) {
        log_error!(self.handle_taker_disconnected(msg.id));
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<RollOverCompleted>
    for Actor<O, M, T, W>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation>,
{
    async fn handle(&mut self, msg: RollOverCompleted, _ctx: &mut Context<Self>) {
        log_error!(self.handle_roll_over_completed(msg));
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<monitor::Event> for Actor<O, M, T, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: monitor::Event, _ctx: &mut Context<Self>) {
        log_error!(self.handle_monitoring_event(msg))
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<FromTaker> for Actor<O, M, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    M: xtra::Handler<monitor::StartMonitoring> + xtra::Handler<monitor::CollaborativeSettlement>,
    T: xtra::Handler<maker_inc_connections::ConfirmOrder>
        + xtra::Handler<maker_inc_connections::TakerMessage>
        + xtra::Handler<maker_inc_connections::BroadcastOrder>
        + xtra::Handler<Stopping<setup_maker::Actor>>
        + xtra::Handler<Stopping<rollover_maker::Actor>>
        + xtra::Handler<maker_inc_connections::settlement::Response>
        + xtra::Handler<Stopping<collab_settlement_maker::Actor>>,
    W: xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>
        + xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, FromTaker { taker_id, msg }: FromTaker, ctx: &mut Context<Self>) {
        match msg {
            wire::TakerToMaker::TakeOrder { order_id, quantity } => {
                log_error!(self.handle_take_order(taker_id, order_id, quantity, ctx))
            }
            wire::TakerToMaker::Settlement {
                order_id,
                msg:
                    wire::taker_to_maker::Settlement::Propose {
                        timestamp,
                        taker,
                        maker,
                        price,
                    },
            } => {
                log_error!(self.handle_propose_settlement(
                    taker_id,
                    SettlementProposal {
                        order_id,
                        timestamp,
                        taker,
                        maker,
                        price
                    },
                    ctx
                ))
            }
            wire::TakerToMaker::Settlement {
                msg: wire::taker_to_maker::Settlement::Initiate { .. },
                ..
            } => {
                unreachable!("Handled within `collab_settlement_maker::Actor");
            }
            wire::TakerToMaker::ProposeRollOver {
                order_id,
                timestamp,
            } => {
                log_error!(self.handle_propose_roll_over(
                    RollOverProposal {
                        order_id,
                        timestamp,
                    },
                    taker_id,
                    ctx
                ))
            }
            wire::TakerToMaker::RollOverProtocol { .. } => {
                unreachable!("This kind of message should be sent to the rollover_maker::Actor`")
            }
            wire::TakerToMaker::Protocol { .. } => {
                unreachable!("This kind of message should be sent to the `setup_maker::Actor`")
            }
            TakerToMaker::Hello(_) => {
                unreachable!("The Hello message is not sent to the cfd actor")
            }
        }
    }
}

#[async_trait]
impl<O: 'static, M: 'static, T: 'static, W: 'static> Handler<oracle::Attestation>
    for Actor<O, M, T, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle(&mut self, msg: oracle::Attestation, _ctx: &mut Context<Self>) {
        log_error!(self.handle_oracle_attestation(msg))
    }
}

impl Message for TakerConnected {
    type Result = ();
}

impl Message for TakerDisconnected {
    type Result = ();
}

impl Message for RollOverCompleted {
    type Result = ();
}

impl Message for FromTaker {
    type Result = ();
}

impl<O: 'static, M: 'static, T: 'static, W: 'static> xtra::Actor for Actor<O, M, T, W> {}
