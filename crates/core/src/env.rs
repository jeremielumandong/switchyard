//! Environment rules: what a connection's environment label means for safety.

use switchyard_db::Dialect;
use switchyard_db::guard::{self, Destructive};
use switchyard_store::{DbConnection, EnvironmentLabel};

/// What to do before running a script on a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Ask the user first (Production, destructive statements). Byte offset of each.
    Confirm(Vec<(usize, Destructive)>),
    /// Refuse (read-only lock).
    Block(String),
}

/// Check a script against the connection's environment and read-only lock.
pub fn check_script(conn: &DbConnection, dialect: &dyn Dialect, script: &str) -> Verdict {
    if conn.read_only {
        for span in dialect.split_script(script) {
            if !guard::classify(dialect, span.text(script)).is_read_only() {
                return Verdict::Block(
                    "This connection is locked read-only; the script would write.".into(),
                );
            }
        }
    }
    if conn.environment == EnvironmentLabel::Production {
        let found = guard::destructive_in_script(dialect, script);
        if !found.is_empty() {
            return Verdict::Confirm(found);
        }
    }
    Verdict::Allow
}

#[cfg(test)]
mod tests {
    use switchyard_db::{Engine, dialect_for};

    use super::*;

    #[test]
    fn production_confirms_and_read_only_blocks() {
        let d = dialect_for(Engine::Postgres);
        let mut c = DbConnection::new("shop_prod", Engine::Postgres);
        assert_eq!(check_script(&c, d, "delete from carts"), Verdict::Allow);
        c.environment = EnvironmentLabel::Production;
        assert!(
            matches!(check_script(&c, d, "select 1;\ndelete from carts"), Verdict::Confirm(v) if v.len() == 1)
        );
        assert_eq!(
            check_script(&c, d, "delete from carts where id = 1"),
            Verdict::Allow
        );
        c.read_only = true;
        assert!(matches!(
            check_script(&c, d, "update t set a = 1 where id = 2"),
            Verdict::Block(_)
        ));
        assert_eq!(check_script(&c, d, "select 1"), Verdict::Allow);
    }
}
