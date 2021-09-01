use crate::cfd::{Cfd, CfdOffer};

use anyhow::Result;
use bitcoin::Amount;
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::tokio::sync::watch::channel;
use rocket::{Config, State};
use tokio::select;
use tokio::sync::{mpsc, watch};

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
        Event::json(&self.as_btc()).event("balance")
    }
}

#[get("/maker-feed")]
async fn maker_feed(
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

#[post("/offer", data = "<cfd_offer>")]
fn post_offer(cfd_offer: Json<CfdOffer>, queue: &State<mpsc::Sender<CfdOffer>>) {
    dbg!(&cfd_offer);

    let _res = queue.send(cfd_offer.into_inner());
}

pub async fn start_http() -> Result<()> {
    let (cfd_feed_sender, cfd_feed_receiver) = channel::<Vec<Cfd>>(vec![]);
    let (_balance_feed_sender, balance_feed_receiver) = channel::<Amount>(Amount::ONE_BTC);

    let (confirm_cfd_sender, mut confirm_cfd_receiver) = mpsc::channel::<Cfd>(1024);

    tokio::spawn({
        async move {
            loop {
                let cfd = confirm_cfd_receiver.recv().await.unwrap();

                // TODO: Communicate with maker to initiate taking CFD...
                //  How would we achieve long running async stuff in here?

                // TODO: Always send all the CFDs to notify the UI
                cfd_feed_sender.send(vec![cfd]).unwrap();
            }
        }
    });

    let config = Config {
        port: 8001,
        ..Config::debug_default()
    };

    rocket::custom(&config)
        .manage(cfd_feed_receiver)
        .manage(confirm_cfd_sender)
        .manage(balance_feed_receiver)
        .mount("/", routes![maker_feed, post_offer])
        .launch()
        .await?;

    Ok(())
}
