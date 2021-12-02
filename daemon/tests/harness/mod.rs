use crate::harness::mocks::monitor::MonitorActor;
use crate::harness::mocks::oracle::OracleActor;
use crate::harness::mocks::wallet::WalletActor;
use crate::schnorrsig;
use ::bdk::bitcoin::Network;
use daemon::bitmex_price_feed::Quote;
use daemon::connection::{connect, ConnectionStatus};
use daemon::model::cfd::Cfd;
use daemon::model::{self, Identity, Price, Timestamp, Usd};
use daemon::projection::{CfdOrder, Feeds};
use daemon::seed::Seed;
use daemon::{
    db, maker_cfd, maker_inc_connections, projection, taker_cfd, MakerActorSystem, Tasks,
    HEARTBEAT_INTERVAL, N_PAYOUTS,
};
use rust_decimal_macros::dec;
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::str::FromStr;
use std::task::Poll;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use xtra::Actor;

pub mod bdk;
pub mod flow;
pub mod maia;
pub mod mocks;

pub const HEARTBEAT_INTERVAL_FOR_TEST: Duration = HEARTBEAT_INTERVAL;
const N_PAYOUTS_FOR_TEST: usize = N_PAYOUTS;

fn oracle_pk() -> schnorrsig::PublicKey {
    schnorrsig::PublicKey::from_str(
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
    )
    .unwrap()
}

pub async fn start_both() -> (Maker, Taker) {
    let maker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let maker = Maker::start(&MakerConfig::default(), maker_listener).await;
    let taker = Taker::start(
        &TakerConfig::default(),
        maker.listen_addr,
        maker.identity_pk,
    )
    .await;
    (maker, taker)
}

pub struct MakerConfig {
    oracle_pk: schnorrsig::PublicKey,
    seed: Seed,
    pub heartbeat_interval: Duration,
    n_payouts: usize,
}

impl MakerConfig {
    pub fn with_heartbeat_interval(self, interval: Duration) -> Self {
        Self {
            heartbeat_interval: interval,
            ..self
        }
    }
}

impl Default for MakerConfig {
    fn default() -> Self {
        Self {
            oracle_pk: oracle_pk(),
            seed: Seed::default(),
            heartbeat_interval: HEARTBEAT_INTERVAL_FOR_TEST,
            n_payouts: N_PAYOUTS_FOR_TEST,
        }
    }
}

#[derive(Clone)]
pub struct TakerConfig {
    oracle_pk: schnorrsig::PublicKey,
    seed: Seed,
    pub heartbeat_timeout: Duration,
    n_payouts: usize,
}

impl TakerConfig {
    pub fn with_heartbeat_timeout(self, timeout: Duration) -> Self {
        Self {
            heartbeat_timeout: timeout,
            ..self
        }
    }
}

impl Default for TakerConfig {
    fn default() -> Self {
        Self {
            oracle_pk: oracle_pk(),
            seed: Seed::default(),
            heartbeat_timeout: HEARTBEAT_INTERVAL_FOR_TEST * 2,
            n_payouts: N_PAYOUTS_FOR_TEST,
        }
    }
}

/// Maker Test Setup
pub struct Maker {
    pub system:
        MakerActorSystem<OracleActor, MonitorActor, maker_inc_connections::Actor, WalletActor>,
    pub mocks: mocks::Mocks,
    pub feeds: Feeds,
    pub listen_addr: SocketAddr,
    pub identity_pk: x25519_dalek::PublicKey,
    _tasks: Tasks,
}

impl Maker {
    pub fn cfd_feed(&mut self) -> &mut watch::Receiver<Vec<Cfd>> {
        &mut self.feeds.cfds
    }

    pub fn order_feed(&mut self) -> &mut watch::Receiver<Option<CfdOrder>> {
        &mut self.feeds.order
    }

    pub fn connected_takers_feed(&mut self) -> &mut watch::Receiver<Vec<Identity>> {
        &mut self.feeds.connected_takers
    }

    pub async fn start(config: &MakerConfig, listener: TcpListener) -> Self {
        let db = in_memory_db().await;

        let mut mocks = mocks::Mocks::default();
        let (oracle, monitor, wallet) = mocks::create_actors(&mocks);

        let mut tasks = Tasks::default();

        let (wallet_addr, wallet_fut) = wallet.create(None).run();
        tasks.add(wallet_fut);

        let settlement_interval = time::Duration::hours(24);

        let (identity_pk, identity_sk) = config.seed.derive_identity();

        let (projection_actor, projection_context) = xtra::Context::new(None);

        // system startup sends sync messages, mock them
        mocks.mock_sync_handlers().await;
        let maker = daemon::MakerActorSystem::new(
            db,
            wallet_addr,
            config.oracle_pk,
            |_, _| oracle,
            |_, _| async { Ok(monitor) },
            |channel0, channel1, channel2| {
                maker_inc_connections::Actor::new(
                    channel0,
                    channel1,
                    channel2,
                    identity_sk,
                    config.heartbeat_interval,
                )
            },
            settlement_interval,
            config.n_payouts,
            projection_actor.clone(),
        )
        .await
        .unwrap();

        let dummy_quote = Quote {
            timestamp: Timestamp::now(),
            bid: Price::new(dec!(10000)).unwrap(),
            ask: Price::new(dec!(10000)).unwrap(),
        };

        let (proj_actor, feeds) =
            projection::Actor::new(Role::Maker, Network::Testnet, vec![], dummy_quote);
        tasks.add(projection_context.run(proj_actor));

        let address = listener.local_addr().unwrap();

        let listener_stream = futures::stream::poll_fn(move |ctx| {
            let message = match futures::ready!(listener.poll_accept(ctx)) {
                Ok((stream, address)) => {
                    maker_inc_connections::ListenerMessage::NewConnection { stream, address }
                }
                Err(e) => maker_inc_connections::ListenerMessage::Error { source: e },
            };

            Poll::Ready(Some(message))
        });

        tasks.add(maker.inc_conn_addr.clone().attach_stream(listener_stream));

        Self {
            system: maker,
            feeds,
            identity_pk,
            listen_addr: address,
            mocks,
            _tasks: tasks,
        }
    }

    pub async fn publish_order(&mut self, new_order_params: maker_cfd::NewOrder) {
        self.mocks.mock_monitor_oracle_attestation().await;

        self.system
            .cfd_actor_addr
            .send(new_order_params)
            .await
            .unwrap()
            .unwrap();
    }

    pub async fn reject_take_request(&self, order: CfdOrder) {
        self.system
            .cfd_actor_addr
            .send(maker_cfd::RejectOrder { order_id: order.id })
            .await
            .unwrap()
            .unwrap();
    }

    pub async fn accept_take_request(&self, order: CfdOrder) {
        self.system
            .cfd_actor_addr
            .send(maker_cfd::AcceptOrder { order_id: order.id })
            .await
            .unwrap()
            .unwrap();
    }
}

/// Taker Test Setup
pub struct Taker {
    pub id: Identity,
    pub system: daemon::TakerActorSystem<OracleActor, MonitorActor, WalletActor>,
    pub mocks: mocks::Mocks,
    pub feeds: Feeds,
    _tasks: Tasks,
}

impl Taker {
    pub fn cfd_feed(&mut self) -> &mut watch::Receiver<Vec<Cfd>> {
        &mut self.feeds.cfds
    }

    pub fn order_feed(&mut self) -> &mut watch::Receiver<Option<CfdOrder>> {
        &mut self.feeds.order
    }

    pub fn maker_status_feed(&mut self) -> &mut watch::Receiver<ConnectionStatus> {
        &mut self.system.maker_online_status_feed_receiver
    }

    pub async fn start(
        config: &TakerConfig,
        maker_address: SocketAddr,
        maker_noise_pub_key: x25519_dalek::PublicKey,
    ) -> Self {
        let (identity_pk, identity_sk) = config.seed.derive_identity();

        let db = in_memory_db().await;

        let mut mocks = mocks::Mocks::default();
        let (oracle, monitor, wallet) = mocks::create_actors(&mocks);

        let mut tasks = Tasks::default();

        let (wallet_addr, wallet_fut) = wallet.create(None).run();
        tasks.add(wallet_fut);

        let (projection_actor, projection_context) = xtra::Context::new(None);

        // system startup sends sync messages, mock them
        mocks.mock_sync_handlers().await;
        let taker = daemon::TakerActorSystem::new(
            db,
            wallet_addr,
            config.oracle_pk,
            identity_sk,
            |_, _| oracle,
            |_, _| async { Ok(monitor) },
            config.n_payouts,
            config.heartbeat_timeout,
            Duration::from_secs(10),
            projection_actor,
            Identity::new(maker_noise_pub_key),
        )
        .await
        .unwrap();

        let dummy_quote = Quote {
            timestamp: Timestamp::now(),
            bid: Price::new(dec!(10000)).unwrap(),
            ask: Price::new(dec!(10000)).unwrap(),
        };

        let (proj_actor, feeds) =
            projection::Actor::new(Role::Taker, Network::Testnet, vec![], dummy_quote);
        tasks.add(projection_context.run(proj_actor));

        tasks.add(connect(
            taker.maker_online_status_feed_receiver.clone(),
            taker.connection_actor_addr.clone(),
            maker_noise_pub_key,
            vec![maker_address],
        ));

        Self {
            id: model::Identity::new(identity_pk).into(),
            system: taker,
            feeds,
            mocks,
            _tasks: tasks,
        }
    }

    pub async fn take_order(&self, order: CfdOrder, quantity: Usd) {
        self.system
            .cfd_actor_addr
            .send(taker_cfd::TakeOffer {
                order_id: order.id,
                quantity,
            })
            .await
            .unwrap()
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

pub fn dummy_new_order() -> maker_cfd::NewOrder {
    maker_cfd::NewOrder {
        price: Price::new(dec!(50_000)).expect("unexpected failure"),
        min_quantity: Usd::new(dec!(5)),
        max_quantity: Usd::new(dec!(100)),
    }
}

pub fn init_tracing() -> DefaultGuard {
    let filter = EnvFilter::from_default_env()
        // apply warning level globally
        .add_directive(format!("{}", LevelFilter::WARN).parse().unwrap())
        // log traces from test itself
        .add_directive(
            format!("happy_path={}", LevelFilter::DEBUG)
                .parse()
                .unwrap(),
        )
        .add_directive(format!("taker={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("maker={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("daemon={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("rocket={}", LevelFilter::WARN).parse().unwrap());

    let guard = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .set_default();

    tracing::info!("Running version: {}", env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT"));

    guard
}
