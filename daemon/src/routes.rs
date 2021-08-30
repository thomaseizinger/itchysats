use crate::cfd::{static_cfd_offer, Cfd, CfdOffer, CfdState, CfdTakeRequest, Usd};

use anyhow::Result;
use bitcoin::Amount;
use rocket::{
    response::stream::{Event, EventStream},
    serde::json::Json,
    tokio::sync::watch::channel,
    State,
};
use std::time::SystemTime;
use tokio::{
    select,
    sync::{mpsc, watch},
};

trait ToSseEvent {
    fn to_sse_event(&self) -> Event;
}

impl ToSseEvent for Vec<Cfd> {
    fn to_sse_event(&self) -> Event {
        Event::json(self).event("cfds")
    }
}

impl ToSseEvent for CfdOffer {
    fn to_sse_event(&self) -> Event {
        Event::json(self).event("offer")
    }
}

impl ToSseEvent for Amount {
    fn to_sse_event(&self) -> Event {
        Event::json(&self.as_sat()).event("balance")
    }
}

#[get("/feed")]
async fn feed(
    rx_cfds: &State<watch::Receiver<Vec<Cfd>>>,
    rx_offer: &State<watch::Receiver<CfdOffer>>,
    rx_balance: &State<watch::Receiver<Amount>>,
) -> EventStream![] {
    let mut rx_cfds = rx_cfds.inner().clone();
    let mut rx_offer = rx_offer.inner().clone();
    let mut rx_balance = rx_balance.inner().clone();

    EventStream! {
        let balance = rx_balance.borrow().clone();
        yield balance.to_sse_event();

        let offer = rx_offer.borrow().clone();
        yield offer.to_sse_event();

        let cfds = rx_cfds.borrow().clone();
        yield cfds.to_sse_event();

        loop{
            select! {
                Ok(()) = rx_balance.changed() => {
                    let balance = rx_balance.borrow().clone();
                    yield balance.to_sse_event();
                },
                Ok(()) = rx_offer.changed() => {
                    let offer = rx_offer.borrow().clone();
                    yield offer.to_sse_event();
                }
                Ok(()) = rx_cfds.changed() => {
                    let cfds = rx_cfds.borrow().clone();
                    yield cfds.to_sse_event();
                }
            }
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
    let (_offer_feed_sender, offer_feed_receiver) = channel::<CfdOffer>(static_cfd_offer());
    let (_balance_feed_sender, balance_feed_receiver) = channel::<Amount>(Amount::ZERO);

    let (take_cfd_sender, mut take_cfd_receiver) = mpsc::channel::<Cfd>(1024);

    tokio::spawn({
        async move {
            loop {
                let cfd = take_cfd_receiver.recv().await.unwrap();

                // TODO: Communicate with maker to initiate taking CFD...
                //  How would we achieve long running async stuff in here?

                // TODO: Always send all the CFDs to notify the UI
                cfd_feed_sender.send(vec![cfd]).unwrap();
            }
        }
    });

    rocket::build()
        .manage(offer_feed_receiver)
        .manage(cfd_feed_receiver)
        .manage(take_cfd_sender)
        .manage(balance_feed_receiver)
        .mount("/", routes![feed, post_cfd])
        .launch()
        .await?;

    Ok(())
}
