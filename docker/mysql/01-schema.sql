-- Sample schema for the MySQL integration tests (crates/db/tests/mysql.rs).
CREATE TABLE IF NOT EXISTS customers (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    email VARCHAR(200) NOT NULL UNIQUE,
    segment ENUM('vip', 'repeat') NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
) COMMENT = 'People who order';

CREATE TABLE IF NOT EXISTS orders (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT UNSIGNED NOT NULL,
    total DECIMAL(10, 2) NOT NULL CHECK (total >= 0),
    placed_on DATE NOT NULL,
    notes JSON NULL,
    CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id)
        ON DELETE CASCADE,
    INDEX ix_orders_placed (placed_on, customer_id)
);

CREATE OR REPLACE VIEW big_orders AS
    SELECT o.id, c.email, o.total FROM orders o JOIN customers c ON c.id = o.customer_id
    WHERE o.total > 100;

INSERT INTO customers (email, segment)
WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
SELECT CONCAT('user', i, '@example.com'),
       CASE i % 3 WHEN 0 THEN 'vip' WHEN 1 THEN 'repeat' END
FROM n;

INSERT INTO orders (customer_id, total, placed_on)
SELECT id, (id * 7) % 500, DATE '2026-01-01' + INTERVAL (id % 200) DAY FROM customers;

DELIMITER $$
CREATE PROCEDURE recent_orders(IN since DATE)
BEGIN
    SELECT COUNT(*) AS n FROM orders WHERE placed_on >= since;
    SELECT id, total FROM orders WHERE placed_on >= since ORDER BY id LIMIT 3;
END$$
DELIMITER ;
