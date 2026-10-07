Switchyard gives read-only access to the database connections its user enabled for agents.

- Call `list_connections` first; refer to connections by name only.
- `run_query` runs one SELECT/WITH statement in a read-only transaction with a row cap and a
  timeout. Writes, DDL and multiple statements are refused.
- `explain` returns the estimated plan as text with its hotspots. Actual plans (ANALYZE) need
  the user's approval in the Switchyard app and are refused here.
- `workload` summarizes the busiest statements, table scans and index usage.
- `what_if` (PostgreSQL with HypoPG) plans a statement with hypothetical indexes; nothing is
  created.
- No tool changes the database. Return index and statistics suggestions to the user as SQL text.
