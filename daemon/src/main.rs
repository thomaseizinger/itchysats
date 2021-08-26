mod cfd;
mod routes;

#[macro_use]
extern crate rocket;

use anyhow::Result;

#[rocket::main]
async fn main() -> Result<()> {
    routes::start_http().await?;

    Ok(())
}
