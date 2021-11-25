use crate::model::cfd::{Cfd, CfdEvent, Event, OrderId};
use crate::{db, monitor, oracle, projection, try_continue, wallet};
use anyhow::{Context, Result};
use sqlx::pool::PoolConnection;
use sqlx::Sqlite;

pub async fn insert_cfd_and_send_to_feed(
    _cfd: &Cfd,
    _conn: &mut PoolConnection<Sqlite>,
    _projection_address: &xtra::Address<projection::Actor>,
) -> Result<()> {
    // db::insert_cfd(cfd, conn).await?;
    // projection_address
    //     .send(projection::Update(db::load_all_cfds(conn).await?))
    //     .await?;
    Ok(())
}

pub async fn append_cfd_state(
    _cfd: &Cfd,
    _conn: &mut PoolConnection<Sqlite>,
    _projection_address: &xtra::Address<projection::Actor>,
) -> Result<()> {
    // db::append_cfd_state(cfd, conn).await?;
    // projection_address
    //     .send(projection::Update(db::load_all_cfds(conn).await?))
    // .await?;
    Ok(())
}

pub async fn handle_monitoring_event<W>(
    event: monitor::Event,
    conn: &mut PoolConnection<Sqlite>,
    wallet: &xtra::Address<W>,
    _projection_address: &xtra::Address<projection::Actor>,
) -> Result<()>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    let order_id = event.order_id();

    let cfd = db::load_cfd(order_id, conn).await?;

    let event = match event {
        monitor::Event::LockFinality(_) => todo!(),
        monitor::Event::CommitFinality(_) => todo!(),
        monitor::Event::CloseFinality(_) => todo!(),
        monitor::Event::CetTimelockExpired(_) => cfd.handle_cet_timelock_expired(),
        monitor::Event::CetFinality(_) => todo!(),
        monitor::Event::RefundTimelockExpired(_) => cfd.handle_refund_timelock_expired(),
        monitor::Event::RefundFinality(_) => todo!(),
        monitor::Event::RevokedTransactionFound(_) => todo!(),
    };

    post_process_event(event, wallet).await?;

    Ok(())
}

pub async fn handle_commit<W>(
    order_id: OrderId,
    conn: &mut PoolConnection<Sqlite>,
    wallet: &xtra::Address<W>,
    _projection_address: &xtra::Address<projection::Actor>,
) -> Result<()>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    let cfd = db::load_cfd(order_id, conn).await?;

    let event = cfd.commit_to_blockchain()?;

    post_process_event(event, &wallet).await?;

    // TODO: Update UI.

    Ok(())
}

pub async fn handle_oracle_attestation<W>(
    attestation: oracle::Attestation,
    conn: &mut PoolConnection<Sqlite>,
    wallet: &xtra::Address<W>,
    _projection_address: &xtra::Address<projection::Actor>,
) -> Result<()>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    tracing::debug!(
        "Learnt latest oracle attestation for event: {}",
        attestation.id
    );

    for cfd in db::load_all_cfds(conn).await? {
        let event = try_continue!(cfd
            .decrypt_cet(&attestation)
            .context("Failed to decrypt CET using attestation"));

        try_continue!(db::append_events(event.clone(), conn)
            .await
            .context("Failed to append events"));

        if let Some(event) = event {
            try_continue!(post_process_event(event, wallet).await)
        }
    }

    // TODO: Update UI.

    Ok(())
}

async fn post_process_event<W>(event: Event, wallet: &xtra::Address<W>) -> Result<()>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    match event.event {
        CfdEvent::OracleAttestedPostCetTimelock { cet }
        | CfdEvent::CetTimelockConfirmedPostOracleAttestation { cet } => {
            let txid = wallet
                .send(wallet::TryBroadcastTransaction { tx: cet })
                .await?
                .context("Failed to broadcast CET")?;

            tracing::info!(%txid, "CET published");
        }
        CfdEvent::Committed { tx } => {
            let txid = wallet
                .send(wallet::TryBroadcastTransaction { tx })
                .await?
                .context("Failed to broadcast commit transaction")?;

            tracing::info!(%txid, "Commit transaction published");
        }

        _ => {} // TODO: Add more cases here.
    }

    Ok(())
}
