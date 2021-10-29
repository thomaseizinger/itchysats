use crate::harness::mocks::monitor::Monitor;
use crate::harness::mocks::oracle::Oracle;
use crate::harness::mocks::wallet::Wallet;
use crate::schnorrsig;
use daemon::maker_cfd::CfdAction;
use daemon::model::cfd::{Cfd, Order};
use daemon::model::Usd;
use daemon::{connection, db, maker_cfd, maker_inc_connections, taker_cfd};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::str::FromStr;
use std::task::Poll;
use tokio::sync::watch;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;

pub mod bdk;
pub mod cfd_protocol;
pub mod mocks;

// TODO: Remove all these parameters once we have the mock framework (because we have
// Arcs then and can just default inside)
pub async fn start_both(
    taker_oracle: Oracle,
    taker_monitor: Monitor,
    taker_wallet: Wallet,
    maker_oracle: Oracle,
    maker_monitor: Monitor,
    maker_wallet: Wallet,
) -> (Maker, Taker) {
    let oracle_pk: schnorrsig::PublicKey = schnorrsig::PublicKey::from_str(
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
    )
    .unwrap();

    let maker = Maker::start(oracle_pk, maker_oracle, maker_monitor, maker_wallet).await;
    let taker = Taker::start(
        oracle_pk,
        maker.listen_addr,
        taker_oracle,
        taker_monitor,
        taker_wallet,
    )
    .await;
    (maker, taker)
}

/// Maker Test Setup
#[derive(Clone)]
pub struct Maker {
    pub cfd_actor_addr:
        xtra::Address<maker_cfd::Actor<Oracle, Monitor, maker_inc_connections::Actor, Wallet>>,
    pub order_feed: watch::Receiver<Option<Order>>,
    pub cfd_feed: watch::Receiver<Vec<Cfd>>,
    #[allow(dead_code)] // we need to keep the xtra::Address for refcounting
    pub inc_conn_actor_addr: xtra::Address<maker_inc_connections::Actor>,
    pub listen_addr: SocketAddr,
}

impl Maker {
    pub async fn start(
        oracle_pk: schnorrsig::PublicKey,
        oracle: Oracle,
        monitor: Monitor,
        wallet: Wallet,
    ) -> Self {
        let db = in_memory_db().await;

        let wallet_addr = wallet.create(None).spawn_global();

        let settlement_time_interval_hours = time::Duration::hours(24);

        let maker = daemon::MakerActorSystem::new(
            db,
            wallet_addr,
            oracle_pk,
            |_, _| oracle,
            |_, _| async { Ok(monitor) },
            |channel0, channel1| maker_inc_connections::Actor::new(channel0, channel1),
            settlement_time_interval_hours,
        )
        .await
        .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let address = listener.local_addr().unwrap();

        let listener_stream = futures::stream::poll_fn(move |ctx| {
            let message = match futures::ready!(listener.poll_accept(ctx)) {
                Ok((stream, address)) => {
                    dbg!("new connection");
                    maker_inc_connections::ListenerMessage::NewConnection { stream, address }
                }
                Err(e) => maker_inc_connections::ListenerMessage::Error { source: e },
            };

            Poll::Ready(Some(message))
        });

        tokio::spawn(maker.inc_conn_addr.clone().attach_stream(listener_stream));

        Self {
            cfd_actor_addr: maker.cfd_actor_addr,
            order_feed: maker.order_feed_receiver,
            cfd_feed: maker.cfd_feed_receiver,
            inc_conn_actor_addr: maker.inc_conn_addr,
            listen_addr: address,
        }
    }

    pub fn publish_order(&mut self, new_order_params: maker_cfd::NewOrder) {
        self.cfd_actor_addr.do_send(new_order_params).unwrap();
    }

    pub fn reject_take_request(&self, order: Order) {
        self.cfd_actor_addr
            .do_send(CfdAction::RejectOrder { order_id: order.id })
            .unwrap();
    }

    pub fn accept_take_request(&self, order: Order) {
        self.cfd_actor_addr
            .do_send(CfdAction::AcceptOrder { order_id: order.id })
            .unwrap();
    }
}

/// Taker Test Setup
#[derive(Clone)]
pub struct Taker {
    pub order_feed: watch::Receiver<Option<Order>>,
    pub cfd_feed: watch::Receiver<Vec<Cfd>>,
    pub cfd_actor_addr: xtra::Address<taker_cfd::Actor<Oracle, Monitor, Wallet>>,
}

impl Taker {
    pub async fn start(
        oracle_pk: schnorrsig::PublicKey,
        maker_address: SocketAddr,
        oracle: Oracle,
        monitor: Monitor,
        wallet: Wallet,
    ) -> Self {
        let connection::Actor {
            send_to_maker,
            read_from_maker,
        } = connection::Actor::new(maker_address).await;

        let db = in_memory_db().await;

        let wallet_addr = wallet.create(None).spawn_global();

        let taker = daemon::TakerActorSystem::new(
            db,
            wallet_addr,
            oracle_pk,
            send_to_maker,
            read_from_maker,
            |_, _| oracle,
            |_, _| async { Ok(monitor) },
        )
        .await
        .unwrap();

        Self {
            order_feed: taker.order_feed_receiver,
            cfd_feed: taker.cfd_feed_receiver,
            cfd_actor_addr: taker.cfd_actor_addr,
        }
    }

    pub fn take_order(&self, order: Order, quantity: Usd) {
        self.cfd_actor_addr
            .do_send(taker_cfd::TakeOffer {
                order_id: order.id,
                quantity,
            })
            .unwrap();
    }
}

async fn in_memory_db() -> SqlitePool {
    // Note: Every :memory: database is distinct from every other. So, opening two database
    // connections each with the filename ":memory:" will create two independent in-memory
    // databases. see: https://www.sqlite.org/inmemorydb.html
    let pool = SqlitePool::connect(":memory:").await.unwrap();

    db::run_migrations(&pool).await.unwrap();

    pool
}