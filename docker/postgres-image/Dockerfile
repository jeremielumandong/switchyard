# PostgreSQL 16 for the integration tests, with HypoPG (hypothetical indexes, M5-8) from
# the PGDG repository the official image already configures.
FROM postgres:16
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-16-hypopg \
 && rm -rf /var/lib/apt/lists/*
