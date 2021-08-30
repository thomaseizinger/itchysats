use crate::cfd::{static_cfd_offer, Cfd, CfdOffer, CfdState, CfdTakeRequest, Usd};

use anyhow::Result;
use bitcoin::Amount;
use rocket::{
    response::stream::{Event, EventStream},
    serde::json::Json,
    tokio::{
        select,
        sync::watch::{channel, error::RecvError, Sender},
    },
    Request, Response, Shutdown, State,
};
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[get("/cfd/feed")]
async fn cfd_feed(rx: &State<watch::Receiver<Vec<Cfd>>>) -> EventStream![] {
    let mut rx = rx.inner().clone();

    EventStream! {
        let cfds = rx.borrow().clone();
        yield Event::json(&cfds).event("cfds");

        while rx.changed().await.is_ok() {
            let cfds = rx.borrow().clone();
            yield Event::json(&cfds).event("cfds");
        }
    }
}

#[get("/cfd/offer/feed")]
async fn cfd_offer_feed(rx: &State<watch::Receiver<CfdOffer>>) -> EventStream![] {
    let mut rx = rx.inner().clone();

    EventStream! {
        let cfd_offer = rx.borrow().clone();
        yield Event::json(&cfd_offer).event("offer");

        while rx.changed().await.is_ok() {
            let cfd_offer = rx.borrow().clone();
            yield Event::json(&cfd_offer).event("offer");
        }
    }
}

#[post("/cfd", data = "<cfd_take_request>")]
fn post_cfd(cfd_take_request: Json<CfdTakeRequest>, queue: &State<mpsc::Sender<Cfd>>) {
    dbg!(cfd_take_request.clone());

    // TODO: Keep the offer of the maker around n state + separate task that keeps it up to date ...
    //  Just use the database...
    let current_offer = static_cfd_offer();

    let cfd = Cfd {
        initial_price: current_offer.price,
        leverage: current_offer.leverage,
        trading_pair: current_offer.trading_pair,
        liquidation_price: current_offer.liquidation_price,
        // TODO calculate btc value from quantity and price
        quantity_btc: Amount::ZERO,
        quantity_usd: cfd_take_request.quantity.clone(),
        profit_btc: Amount::ZERO,
        profit_usd: Usd(0),
        creation_date: SystemTime::now(),
        state: CfdState::TakeRequested,
    };

    let _res = queue.send(cfd);
}

pub async fn start_http() -> Result<()> {
    let (cfd_feed_sender, cfd_feed_receiver) = channel::<Vec<Cfd>>(vec![]);
    let (offer_feed_sender, offer_feed_receiver) = channel::<CfdOffer>(static_cfd_offer());

    let (take_cfd_sender, mut take_cfd_receiver) = mpsc::channel::<Cfd>(1024);

    tokio::spawn({
        async move {
            loop {
                let cfd = take_cfd_receiver.recv().await.unwrap();

                // TODO: Communicate with maker to initiate taking CFD...
                //  How would we achieve long running async stuff in here?

                // TODO: Always send all the CFDs to notify the UI
                cfd_feed_sender.send(vec![cfd]);
            }
        }
    });

    rocket::build()
        .manage(offer_feed_receiver)
        .manage(cfd_feed_receiver)
        .manage(take_cfd_sender)
        .mount("/", routes![cfd_offer_feed, cfd_feed, post_cfd])
        .launch()
        .await?;

    Ok(())
}
