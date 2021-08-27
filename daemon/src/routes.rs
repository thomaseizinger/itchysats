use crate::cfd::{static_cfd_offer, Cfd, CfdState, CfdTakeRequest, Usd};

use bitcoin::Amount;
use rocket::form::Form;
use rocket::fs::{relative, FileServer};
use rocket::response::stream::{Event, EventStream};
use rocket::tokio::select;
use rocket::tokio::sync::broadcast::{channel, error::RecvError, Sender};
use rocket::{Shutdown, State};
use rust_decimal_macros::dec;
use std::time::SystemTime;

/// Returns an infinite stream of server-sent events. Each event is a message
/// pulled from a broadcast queue sent by the `post` handler.
#[get("/cfds")]
async fn events(queue: &State<Sender<Cfd>>, mut end: Shutdown) -> EventStream![] {
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
fn post_cfd(cfd_take_request: CfdTakeRequest, queue: &State<Sender<Cfd>>) {
    // TODO: Communicate with maker to initiate taking CFD...

    // TODO: Keep the offer of the maker around on disk, separate task that keeps it up to date ...
    let current_offer = static_cfd_offer();

    let cfd = Cfd {
        initial_price: current_offer.price,
        leverage: current_offer.leverage,
        trading_pair: current_offer.trading_pair,
        liquidation_price: current_offer.liquidation_price,
        quantity_btc: todo!("calculate btc value from quantity and price"),
        quantity_usd: cfd_take_request.quantity,
        profit_btc: Amount::ZERO,
        profit_usd: Usd(0),
        creation_date: SystemTime::now(),
        state: CfdState::TakeRequested,
    };

    let _res = queue.send(cfd);
}
