use crate::cfd::{static_cfd_offer, Cfd, CfdState, CfdTakeRequest, Usd};

use bitcoin::Amount;
use rocket::{
    response::stream::{Event, EventStream},
    serde::json::Json,
    tokio::{
        select,
        sync::broadcast::{error::RecvError, Sender},
    },
    Shutdown, State,
};
use rust_decimal_macros::dec;
use std::time::SystemTime;

/// Returns an infinite stream of server-sent events. Each event is a message
/// pulled from a broadcast queue sent by the `post` handler.
#[get("/cfds")]
async fn cfd_stream(queue: &State<Sender<Cfd>>, mut end: Shutdown) -> EventStream![] {
    let mut rx = queue.subscribe();
    EventStream! {
        loop {
            let msg = select! {
                msg = rx.recv() => match msg {
                    Ok(msg) => msg,
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                },
                _ = &mut end => break,
            };

            yield Event::json(&msg);
        }
    }
}

/// Receive a cfd from a form submission and broadcast it to any receivers.
#[post("/cfd", data = "<cfd_take_request>")]
fn post_cfd(cfd_take_request: Json<CfdTakeRequest>, queue: &State<Sender<Cfd>>) {
    // TODO: Communicate with maker to initiate taking CFD...
    //  How would we achieve long running async stuff in here?

    // TODO: Keep the offer of the maker around n state + separate task that keeps it up to date ...
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
