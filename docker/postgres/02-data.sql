-- Deterministic sample data, including a 1,000,000-row orders table.
INSERT INTO customers (email, segment, created_at)
SELECT 'user' || g || '@example.com',
       (ARRAY['vip', 'repeat', NULL, 'new'])[1 + g % 4],
       timestamptz '2024-01-01' + (g || ' minutes')::interval
FROM generate_series(1, 84000) g;

INSERT INTO products (sku, name, price)
SELECT 'SKU-' || g, 'Product ' || g, (g % 500) + 0.99
FROM generate_series(1, 2100) g;

INSERT INTO orders (customer_id, status, total, created_at)
SELECT 1 + (g::bigint * 7919) % 84000,
       (ARRAY['new', 'paid', 'shipped', 'cancelled'])[1 + g % 4],
       ((g::bigint * 37) % 100000) / 100.0,
       timestamptz '2025-01-01' + ((g % 400) || ' days')::interval + ((g % 1440) || ' minutes')::interval
FROM generate_series(1, 1000000) g;

INSERT INTO order_items (order_id, product_id, quantity)
SELECT g, 1 + g % 2100, 1 + g % 5 FROM generate_series(1, 200000) g;

INSERT INTO payments (order_id, amount, paid_at)
SELECT g, ((g::bigint * 37) % 100000) / 100.0, timestamptz '2025-01-02' + ((g % 400) || ' days')::interval
FROM generate_series(1, 100000) g;

INSERT INTO abandoned_carts (customer_id)
SELECT 1 + g % 84000 FROM generate_series(1, 48210) g;

REFRESH MATERIALIZED VIEW daily_revenue;
ANALYZE;
