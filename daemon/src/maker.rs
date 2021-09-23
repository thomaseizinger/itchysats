use crate::auth::MAKER_USERNAME;
use crate::maker_inc_connections::in_taker_messages;
use crate::model::TakerId;
use crate::seed::Seed;
use crate::wallet::Wallet;
use anyhow::{Context, Result};
use bdk::bitcoin::secp256k1::{schnorrsig, SECP256K1};
use bdk::bitcoin::Network;
use clap::Clap;
use model::cfd::{Cfd, Order};
use model::WalletInfo;
use rocket::fairing::AdHoc;
use rocket_db_pools::Database;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
use tracing_subscriber::filter::LevelFilter;
use xtra::prelude::*;
use xtra::spawn::TokioGlobalSpawnExt;

mod actors;
mod auth;
mod bitmex_price_feed;
mod db;
mod keypair;
mod logger;
mod maker_cfd_actor;
mod maker_inc_connections;
mod model;
mod routes;
mod routes_maker;
mod seed;
mod send_wire_message_actor;
mod setup_contract_actor;
mod to_sse_event;
mod wallet;
mod wire;

#[derive(Database)]
#[database("maker")]
pub struct Db(sqlx::SqlitePool);

#[derive(Clap)]
struct Opts {
    /// The port to listen on for p2p connections.
    #[clap(long, default_value = "9999")]
    p2p_port: u16,

    /// The port to listen on for the HTTP API.
    #[clap(long, default_value = "8001")]
    http_port: u16,

    /// URL to the electrum backend to use for the wallet.
    #[clap(long, default_value = "ssl://electrum.blockstream.info:60002")]
    electrum: String,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    data_dir: Option<PathBuf>,

    /// Generate a seed file within the data directory.
    #[clap(long)]
    generate_seed: bool,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    json: bool,
}

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    logger::init(LevelFilter::DEBUG, opts.json).context("initialize logger")?;

    let data_dir = opts
        .data_dir
        .unwrap_or_else(|| std::env::current_dir().expect("unable to get cwd"));

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let seed = Seed::initialize(&data_dir.join("maker_seed"), opts.generate_seed).await?;

    let ext_priv_key = seed.derive_extended_priv_key(Network::Testnet)?;

    let wallet = Wallet::new(
        &opts.electrum,
        &data_dir.join("maker_wallet_db"),
        ext_priv_key,
    )
    .await?;
    let wallet_info = wallet.sync().await.unwrap();

    let auth_password = seed.derive_auth_password::<auth::Password>();

    tracing::info!(
        "Authentication details: username='{}' password='{}'",
        MAKER_USERNAME,
        auth_password
    );

    let oracle = schnorrsig::KeyPair::new(SECP256K1, &mut rand::thread_rng()); // TODO: Fetch oracle public key from oracle.

    let (cfd_feed_sender, cfd_feed_receiver) = watch::channel::<Vec<Cfd>>(vec![]);
    let (order_feed_sender, order_feed_receiver) = watch::channel::<Option<Order>>(None);
    let (wallet_feed_sender, wallet_feed_receiver) = watch::channel::<WalletInfo>(wallet_info);

    let figment = rocket::Config::figment()
        .merge(("databases.maker.url", data_dir.join("maker.sqlite")))
        .merge(("port", opts.http_port));

    let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{}", opts.p2p_port)).await?;
    let local_addr = listener.local_addr().unwrap();

    tracing::info!("Listening on {}", local_addr);

    let (task, mut quote_updates) = bitmex_price_feed::new().await?;
    tokio::spawn(task);

    // dummy usage of quote receiver
    tokio::spawn(async move {
        loop {
            let bitmex_price_feed::Quote { bid, ask, .. } = *quote_updates.borrow();
            tracing::info!(%bid, %ask, "BitMex quote updated");

            if quote_updates.changed().await.is_err() {
                return;
            }
        }
    });

    rocket::custom(figment)
        .manage(cfd_feed_receiver)
        .manage(order_feed_receiver)
        .manage(wallet_feed_receiver)
        .manage(auth_password)
        .attach(Db::init())
        .attach(AdHoc::try_on_ignite(
            "SQL migrations",
            |rocket| async move {
                match Db::fetch(&rocket) {
                    Some(db) => match db::run_migrations(&**db).await {
                        Ok(_) => Ok(rocket),
                        Err(_) => Err(rocket),
                    },
                    None => Err(rocket),
                }
            },
        ))
        .attach(AdHoc::try_on_ignite(
            "Create actors",
            move |rocket| async move {
                let db = match Db::fetch(&rocket) {
                    Some(db) => (**db).clone(),
                    None => return Err(rocket),
                };

                let (maker_inc_connections_address, maker_inc_connections_context) =
                    xtra::Context::new(None);

                let cfd_maker_actor_inbox = maker_cfd_actor::MakerCfdActor::new(
                    db,
                    wallet,
                    schnorrsig::PublicKey::from_keypair(SECP256K1, &oracle),
                    cfd_feed_sender,
                    order_feed_sender,
                    wallet_feed_sender,
                    maker_inc_connections_address.clone(),
                )
                .await
                .unwrap()
                .create(None)
                .spawn_global();

                tokio::spawn(
                    maker_inc_connections_context.run(maker_inc_connections::Actor::new(
                        cfd_maker_actor_inbox.clone(),
                    )),
                );

                tokio::spawn({
                    let cfd_maker_actor_inbox = cfd_maker_actor_inbox.clone();
                    let maker_inc_connections_address = maker_inc_connections_address.clone();
                    async move {
                        loop {
                            if let Ok((socket, remote_addr)) = listener.accept().await {
                                tracing::info!("Connected to {}", remote_addr);
                                let taker_id = TakerId::default();
                                let (read, write) = socket.into_split();

                                let in_taker_actor = in_taker_messages(
                                    read,
                                    cfd_maker_actor_inbox.clone(),
                                    taker_id,
                                );
                                let (out_msg_actor, out_msg_actor_inbox) =
                                    send_wire_message_actor::new::<wire::MakerToTaker>(write);

                                tokio::spawn(in_taker_actor);
                                tokio::spawn(out_msg_actor);

                                maker_inc_connections_address
                                    .do_send_async(maker_inc_connections::NewTakerOnline {
                                        taker_id,
                                        out_msg_actor_inbox,
                                    })
                                    .await
                                    .unwrap();
                            };
                        }
                    }
                });

                // consecutive wallet syncs handled by task that triggers sync
                let wallet_sync_interval = Duration::from_secs(10);
                tokio::spawn({
                    let cfd_actor_inbox = cfd_maker_actor_inbox.clone();
                    async move {
                        loop {
                            cfd_actor_inbox
                                .do_send_async(maker_cfd_actor::SyncWallet)
                                .await
                                .unwrap();
                            tokio::time::sleep(wallet_sync_interval).await;
                        }
                    }
                });

                Ok(rocket.manage(cfd_maker_actor_inbox))
            },
        ))
        .mount(
            "/api",
            rocket::routes![
                routes_maker::maker_feed,
                routes_maker::post_sell_order,
                routes_maker::post_accept_order,
                routes_maker::post_reject_order,
                routes_maker::get_health_check
            ],
        )
        .register("/api", rocket::catchers![routes_maker::unauthorized])
        .mount(
            "/",
            rocket::routes![routes_maker::dist, routes_maker::index],
        )
        .register("/", rocket::catchers![routes_maker::unauthorized])
        .launch()
        .await?;

    Ok(())
}
