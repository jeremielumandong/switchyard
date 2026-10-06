-- Sample schema for integration tests and demos.
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

CREATE TABLE customers (
    id          bigserial PRIMARY KEY,
    email       text NOT NULL UNIQUE,
    segment     text,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE products (
    id      serial PRIMARY KEY,
    sku     text NOT NULL UNIQUE,
    name    text NOT NULL,
    price   numeric(12,2) NOT NULL
);

CREATE TABLE orders (
    id           bigserial PRIMARY KEY,
    customer_id  bigint NOT NULL REFERENCES customers(id),
    status       text NOT NULL DEFAULT 'new',
    total        numeric(12,2) NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX orders_customer_id_idx ON orders (customer_id);

CREATE TABLE order_items (
    order_id    bigint NOT NULL REFERENCES orders(id),
    product_id  int NOT NULL REFERENCES products(id),
    quantity    int NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

CREATE TABLE payments (
    id        bigserial PRIMARY KEY,
    order_id  bigint NOT NULL REFERENCES orders(id),
    amount    numeric(12,2) NOT NULL,
    paid_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE abandoned_carts (
    id           bigserial PRIMARY KEY,
    customer_id  bigint REFERENCES customers(id),
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE VIEW top_customers AS
SELECT c.id, c.email, count(o.id) AS orders, sum(o.total) AS revenue
FROM customers c JOIN orders o ON o.customer_id = c.id
GROUP BY c.id, c.email;

CREATE MATERIALIZED VIEW daily_revenue AS
SELECT date_trunc('day', created_at) AS day, sum(total) AS revenue
FROM orders GROUP BY 1;

CREATE TYPE order_status AS ENUM ('new', 'paid', 'shipped', 'cancelled');

CREATE FUNCTION customer_revenue(cid bigint) RETURNS numeric
LANGUAGE sql STABLE AS $$ SELECT coalesce(sum(total), 0) FROM orders WHERE customer_id = cid $$;

-- One row per supported type, for driver type-mapping tests.
CREATE TABLE type_samples (
    id      int PRIMARY KEY,
    b       boolean, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8,
    n       numeric(14,4), t text, vc varchar(20), c char(4), by bytea, u uuid,
    j       json, jb jsonb, d date, tm time, ts timestamp, tstz timestamptz, iv interval,
    arr     int4[], st order_status, ip inet
);
INSERT INTO type_samples VALUES
 (1, true, -2, 40000, 9000000000, 1.5, 2.25, 4812.4000, 'text', 'varchar', 'ch',
  '\xdeadbeef', '550e8400-e29b-41d4-a716-446655440000', '{"a": 1}', '{"b": [1, 2]}',
  '2026-10-05', '08:14:22.5', '2026-10-05 08:14:22', '2026-10-05 08:14:22+00',
  '1 year 2 mons 3 days 04:05:06', '{1,NULL,3}', 'paid', '10.0.4.12'),
 (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '', NULL, NULL, NULL, NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
