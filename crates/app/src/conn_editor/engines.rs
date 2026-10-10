//! The database engines the connection editor offers, one module each.
//!
//! Adding an engine takes its module, one entry in [`ALL`] and one arm in [`form`].

use switchyard_core::db::Engine;

use super::form::EngineForm;

mod d1;
mod durable_object;
mod mongo;
mod mysql;
mod oracle;
mod postgres;
mod redis;
mod snowflake;
mod sqlite;
mod sqlserver;

/// Engines in picker order.
pub(super) const ALL: &[Engine] = &[
    Engine::Postgres,
    Engine::SqlServer,
    Engine::MySql,
    Engine::Oracle,
    Engine::Snowflake,
    Engine::D1,
    Engine::DurableObject,
    Engine::Sqlite,
    Engine::MongoDb,
    Engine::Redis,
];

/// The form for `engine`.
pub(super) fn form(engine: Engine) -> &'static dyn EngineForm {
    match engine {
        Engine::Postgres => &postgres::Postgres,
        Engine::SqlServer => &sqlserver::SqlServer,
        Engine::MySql => &mysql::MySql,
        Engine::Oracle => &oracle::Oracle,
        Engine::Snowflake => &snowflake::Snowflake,
        Engine::D1 => &d1::D1,
        Engine::DurableObject => &durable_object::DurableObject,
        Engine::Sqlite => &sqlite::Sqlite,
        Engine::MongoDb => &mongo::Mongo,
        Engine::Redis => &redis::Redis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_engine_has_its_own_form() {
        for (i, e) in ALL.iter().enumerate() {
            assert_eq!(form(*e).engine(), *e);
            assert!(!ALL[..i].contains(e), "{e:?} is listed twice");
        }
    }
}
