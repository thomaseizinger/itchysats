use crate::cfd::{static_cfd_offer, Cfd, CfdOffer, CfdState, CfdTakeRequest, Usd};

use anyhow::Result;
use bitcoin::Amount;
use rocket::{
    fairing::{Fairing, Info, Kind},
    http::Header,
    response::stream::EventStream,
    serde::json::Json,
    tokio::{
        select,
        sync::watch::{channel, error::RecvError, Sender},
    },
    Request, Response, Shutdown, State,
};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use tokio::sync::{mpsc, mpsc::Receiver, watch};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Event {
    Cfd(Cfd),
    Offer(CfdOffer),
}

impl Event {
    pub fn to_sse_event(&self) -> rocket::response::stream::Event {
        match self {
            Event::Cfd(cfd) => rocket::response::stream::Event::json(&cfd).event("cfd"),
            Event::Offer(offer) => rocket::response::stream::Event::json(&offer).event("offer"),
        }
    }
}

#[get("/feed")]
async fn events(rx: &State<watch::Receiver<Event>>, mut end: Shutdown) -> EventStream![] {
    let mut rx = rx.inner().clone();

    EventStream! {
        let event = rx.borrow().clone();
        yield event.to_sse_event();

        while rx.changed().await.is_ok() {
            let event = rx.borrow().clone();
            yield event.to_sse_event();
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
    let (feed_sender, feed_receiver) = channel::<Event>(Event::Offer(static_cfd_offer()));
    let (cfd_sender, mut cfd_receiver) = mpsc::channel::<Cfd>(1024);

    tokio::spawn({
        async move {
            loop {
                let cfd = cfd_receiver.recv().await.unwrap();

                // TODO: Communicate with maker to initiate taking CFD...
                //  How would we achieve long running async stuff in here?

                feed_sender.send(Event::Cfd(cfd));
            }
        }
    });

    rocket::build()
        .manage(feed_receiver)
        .manage(cfd_sender)
        .mount("/", routes![events, post_cfd])
        .launch()
        .await?;

    Ok(())
}
