//! Coding agents' actual plans. An actual plan (`EXPLAIN ANALYZE`, `STATISTICS XML`) runs
//! the statement, so `swy mcp` asks the app first ([`crate::handoff::ask_agent_plan`]): the
//! app checks the run's session token and the connection, shows the exact statement
//! ([`Event::AgentApproval`](crate::Event::AgentApproval)) and answers. On approval `swy mcp`
//! captures the plan on its own session, where writes run inside a transaction that is
//! rolled back, and records the call in history. Production connections stay estimated only.

use std::sync::Arc;

use switchyard_db::guard;
use switchyard_db::{Engine, dialect_for};
use switchyard_store::{DbConnection, Profile};

use super::Service;
use super::agent_ssh::Answer;
use crate::agent_run::verify_token;
use crate::bus::{AgentApproval, ApprovalKind};
use crate::handoff::{AgentPlan, PlanApprover};

impl Service {
    /// The handoff listener's answer to [`AgentPlan`]s.
    pub(super) fn plan_approver(self: &Arc<Self>) -> PlanApprover {
        let weak = Arc::downgrade(self);
        Arc::new(move |req| {
            let weak = weak.clone();
            Box::pin(async move {
                let Some(this) = weak.upgrade() else {
                    return Err("Switchyard is closing".to_owned());
                };
                this.agent_plan(req).await
            })
        })
    }

    async fn agent_plan(&self, req: AgentPlan) -> Result<(), String> {
        let scope = verify_token(&self.data_dir, &req.session_token).map_err(|e| e.to_string())?;
        let name = req.connection.trim().to_owned();
        let profiles = self
            .with_store(|s| s.profiles())
            .await
            .map_err(|e| e.to_string())?;
        let conn = agent_connection(&profiles, &name, |c| scope.allows(&c.id))
            // Same answer for "missing" and "not enabled".
            .ok_or_else(|| format!("no connection named {name:?} is available to agents"))?;
        let sql = req.sql.trim().to_owned();
        let writes = plan_refusal(&conn, &sql)?;
        let answer = self
            .ask_user(AgentApproval {
                id: 0,
                agent: scope.agent,
                kind: ApprovalKind::ActualPlan { writes },
                target: conn.id.clone(),
                target_name: conn.name.clone(),
                environment: conn.environment,
                text: sql,
            })
            .await;
        match answer {
            Answer::Approved => Ok(()),
            Answer::Declined => Err("the user declined to run this actual plan".to_owned()),
            Answer::TimedOut => {
                Err("nobody approved the actual plan in Switchyard in time".to_owned())
            }
        }
    }
}

/// The agent-enabled connection called `name` (case-insensitive) that `in_scope` allows.
fn agent_connection(
    profiles: &[Profile],
    name: &str,
    in_scope: impl Fn(&DbConnection) -> bool,
) -> Option<DbConnection> {
    profiles.iter().find_map(|p| match p {
        Profile::Db(c) if c.agent_access && in_scope(c) && c.name.eq_ignore_ascii_case(name) => {
            Some(c.clone())
        }
        _ => None,
    })
}

/// Whether an agent may ask for an actual plan of `sql` on `conn`: `Ok(writes)` (the
/// statement writes, rolled back), or why not. Production connections are estimated only.
pub fn plan_refusal(conn: &DbConnection, sql: &str) -> Result<bool, String> {
    if conn.environment.is_production() {
        return Err(format!(
            "{} is a Production connection: agents get estimated plans only there",
            conn.name
        ));
    }
    if !matches!(conn.engine, Engine::Postgres | Engine::SqlServer) {
        return Err(format!(
            "actual plans are available on PostgreSQL and SQL Server only; {} is {}",
            conn.name,
            conn.engine.display_name()
        ));
    }
    let dialect = dialect_for(conn.engine);
    if !guard::is_single_plannable(dialect, sql) {
        return Err("explain takes one SELECT, INSERT, UPDATE, DELETE or MERGE statement".into());
    }
    let writes = !guard::classify(dialect, sql).is_read_only();
    if writes && conn.read_only {
        return Err(format!(
            "{} is locked read-only; an actual plan would run this write (rolled back, but \
             triggers and sequences still fire). Use the estimated plan",
            conn.name
        ));
    }
    Ok(writes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use switchyard_store::EnvironmentLabel;

    #[test]
    fn only_agent_enabled_connections_in_scope() {
        let mut shop = DbConnection::new("shop", Engine::Postgres);
        shop.agent_access = true;
        let admin = DbConnection::new("admin", Engine::Postgres);
        let profiles = vec![Profile::Db(shop.clone()), Profile::Db(admin)];
        assert_eq!(
            agent_connection(&profiles, "SHOP", |_| true).map(|c| c.id),
            Some(shop.id.clone())
        );
        assert!(agent_connection(&profiles, "shop", |_| false).is_none());
        assert!(agent_connection(&profiles, "admin", |_| true).is_none());
        assert!(agent_connection(&profiles, "nope", |_| true).is_none());
    }

    #[test]
    fn actual_plans_refused_where_they_must_be() {
        let mut shop = DbConnection::new("shop", Engine::Postgres);
        assert_eq!(plan_refusal(&shop, "select * from orders"), Ok(false));
        assert_eq!(
            plan_refusal(&shop, "delete from orders where id = 1"),
            Ok(true)
        );
        assert!(plan_refusal(&shop, "drop table orders").is_err());
        assert!(plan_refusal(&shop, "select 1; delete from orders").is_err());

        shop.read_only = true;
        assert_eq!(plan_refusal(&shop, "select 1"), Ok(false));
        assert!(
            plan_refusal(&shop, "update orders set total = 0")
                .unwrap_err()
                .contains("read-only")
        );

        let mut prod = DbConnection::new("prod", Engine::SqlServer);
        prod.environment = EnvironmentLabel::Production;
        assert!(
            plan_refusal(&prod, "select 1")
                .unwrap_err()
                .contains("estimated plans only")
        );

        let lite = DbConnection::new("local", Engine::Sqlite);
        assert!(plan_refusal(&lite, "select 1").is_err());
        let ms = DbConnection::new("ms", Engine::SqlServer);
        assert_eq!(plan_refusal(&ms, "select top 5 * from t"), Ok(false));
    }
}
