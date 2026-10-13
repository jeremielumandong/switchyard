Switchyard gives read-only access to the database connections and SSH hosts its user enabled
for agents.

- Call `list_connections` first; refer to connections and hosts by name only.
- `list_tables` and `describe_table` show tables, views (MongoDB: collections), columns,
  indexes, constraints and foreign keys.
- `run_query` runs one SELECT/WITH statement in a read-only transaction with a row cap
  (default 200, at most 1000) and a timeout. On MongoDB it takes one mongosh statement that
  reads. Writes, DDL and multiple statements are refused.
- `explain` returns the estimated plan as text with its hotspots. An actual plan (`analyze`)
  runs only after the user approves that exact statement in the Switchyard app, never on
  Production; prefer the estimated plan.
- `workload` summarizes the busiest statements, table scans and index usage.
- `what_if` (PostgreSQL with HypoPG) plans a statement with hypothetical indexes; nothing is
  created.
- `redis_command` runs one read-only Redis command (SCAN, TYPE, GET, HGETALL, TTL, …). Writes
  and KEYS are refused. The SQL tools do not work on Redis connections.
- `run_ssh_command` runs one shell command on an SSH host, only in a run the Switchyard app
  started, and only after the user approves that exact command. Keep commands short and
  read-only, and say why you need each one.
- No tool changes the database. Return index and statistics suggestions to the user as SQL text.
