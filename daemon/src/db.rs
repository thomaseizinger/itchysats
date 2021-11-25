use crate::model::cfd::{Aggregate, CfdAggregate, CfdEvent, Event, OrderId};
use anyhow::{Context, Result};
use sqlx::pool::PoolConnection;
use sqlx::{Acquire, Sqlite, SqlitePool};
use time::Duration;

pub async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

pub async fn insert_cfd(cfd: &CfdAggregate, conn: &mut PoolConnection<Sqlite>) -> Result<()> {
    let query_result = sqlx::query(
        r#"
        insert into cfds (
            uuid,
            position,
            initial_price,
            leverage,
            settlement_time_interval_hours,
            quantity_usd,
            counterparty_network_identity,
            role
        ) values ($1, $2, $3, $4, $5, $6, $7, $8)"#,
    )
    .bind(&cfd.id())
    .bind(&cfd.position())
    .bind(&cfd.initial_price())
    .bind(&cfd.leverage())
    .bind(&cfd.settlement_time_interval_hours().whole_hours())
    .bind(&cfd.quantity())
    .bind(&cfd.counterparty_network_identity())
    .execute(conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert cfd");
    }

    Ok(())
}

/// Appends a series of events to the `events` table.
///
/// Implementation is sub-optimal because we need to use transactions. Ideally we would use
/// multi-row inserts but they are unsupported with sqlx.
pub async fn append_events(
    events: impl IntoEvents,
    conn: &mut PoolConnection<Sqlite>,
) -> Result<()> {
    let events = events.into_events();

    let mut transaction = conn.begin().await?;

    for event in events {
        let (event_name, event_data) = event.event.to_json();

        let query_result = sqlx::query(
            r#"
        insert into events (
            cfd_id,
            name,
            data,
            created_at
        ) values (
            (select id from cfds where cfds.uuid = $1),
            $2, $3, $4
        )"#,
        )
        .bind(&event.id)
        .bind(&event_name)
        .bind(&event_data)
        .bind(&event.timestamp)
        .execute(&mut transaction)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert event");
        }
    }

    transaction
        .commit()
        .await
        .context("Failed to commit transaction")?;

    Ok(())
}

/// Trait to make it more convenient to pass different types to `append_events`.
pub trait IntoEvents {
    fn into_events(self) -> Vec<Event>;
}

impl IntoEvents for Event {
    fn into_events(self) -> Vec<Event> {
        vec![self]
    }
}

impl IntoEvents for Option<Event> {
    fn into_events(self) -> Vec<Event> {
        self.map(|e| vec![e]).unwrap_or_default()
    }
}

impl IntoEvents for Vec<Event> {
    fn into_events(self) -> Vec<Event> {
        self
    }
}

pub async fn load_cfd(id: OrderId, conn: &mut PoolConnection<Sqlite>) -> Result<CfdAggregate> {
    let cfd_row = sqlx::query!(
        r#"
            select
                id as cfd_id,
                uuid as "uuid: crate::model::cfd::OrderId",
                position as "position: crate::model::Position",
                initial_price as "initial_price: crate::model::Price",
                leverage as "leverage: crate::model::Leverage",
                settlement_time_interval_hours,
                quantity_usd as "quantity_usd: crate::model::Usd",
                counterparty_network_identity as "counterparty_network_identity: crate::model::Identity",
                role as "role: crate::model::cfd::Role"
            from
                cfds
            where
                cfds.uuid = $1
            "#,
            id
    )
    .fetch_one(&mut *conn)
    .await?;

    let cfd = CfdAggregate::new(
        cfd_row.uuid,
        cfd_row.position,
        cfd_row.initial_price,
        cfd_row.leverage,
        Duration::hours(cfd_row.settlement_time_interval_hours),
        cfd_row.quantity_usd,
        cfd_row.counterparty_network_identity,
        cfd_row.role,
    );

    let events = sqlx::query!(
        r#"

        select
            name,
            data,
            created_at as "created_at: crate::model::Timestamp"
        from
            events
        where
            cfd_id = $1
            "#,
        cfd_row.cfd_id
    )
    .fetch_all(&mut *conn)
    .await?;

    let events = events
        .into_iter()
        .map(|event_row| {
            let cfd_event = CfdEvent::from_json(event_row.name, event_row.data)?;

            Ok(Event {
                timestamp: event_row.created_at,
                id,
                event: cfd_event,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let cfd = events.into_iter().fold(cfd, |cfd, event| cfd.apply(&event));

    Ok(cfd)
}

pub async fn load_all_cfds(conn: &mut PoolConnection<Sqlite>) -> Result<Vec<CfdAggregate>> {
    let records = sqlx::query!(
        r#"
            select
                uuid as "uuid: crate::model::cfd::OrderId"
            from
                cfds
            "#
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut cfds = Vec::with_capacity(records.len());

    for record in records {
        cfds.push(load_cfd(record.uuid, &mut *conn).await?)
    }

    Ok(cfds)
}

#[cfg(test)]
mod tests {
    use crate::model::{Leverage, Position, Price, Usd};
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;
    use sqlx::SqlitePool;

    use super::*;
    use crate::model::cfd::Role;

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let mut conn = setup_test_db().await;

        let cfd = CfdAggregate::dummy().insert(&mut conn).await;
        let loaded = load_cfd(cfd.id(), &mut conn).await.unwrap();

        assert_eq!(cfd, loaded);
    }

    #[tokio::test]
    async fn saved_events_are_applied_to_loaded_cfd() {
        let mut conn = setup_test_db().await;

        let cfd = CfdAggregate::dummy().insert(&mut conn).await;
        append_events(
            vec![Event::new(cfd.id(), CfdEvent::OfferRejected)],
            &mut conn,
        )
        .await
        .unwrap();

        let loaded = load_cfd(cfd.id(), &mut conn).await.unwrap();

        assert_eq!(loaded.version(), 1); // we applied 1 event
    }

    async fn setup_test_db() -> PoolConnection<Sqlite> {
        let pool = SqlitePool::connect(":memory:").await.unwrap();

        run_migrations(&pool).await.unwrap();

        pool.acquire().await.unwrap()
    }

    impl CfdAggregate {
        fn dummy() -> Self {
            CfdAggregate::new(
                OrderId::default(),
                Position::Long,
                Price::new(dec!(60_000)).unwrap(),
                Leverage::new(2).unwrap(),
                Duration::hours(24),
                Usd::new(dec!(1_000)),
                "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
                    .parse()
                    .unwrap(),
                Role::Taker,
            )
        }

        /// Insert this [`Cfd`] into the database, returning the instance for further chaining.
        async fn insert(self, conn: &mut PoolConnection<Sqlite>) -> Self {
            insert_cfd(&self, conn).await.unwrap();

            self
        }
    }
}
