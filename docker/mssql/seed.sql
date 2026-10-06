IF DB_ID('shop') IS NULL CREATE DATABASE shop;
GO
USE shop;
GO
IF OBJECT_ID('dbo.customers') IS NULL
CREATE TABLE dbo.customers (
    id INT IDENTITY PRIMARY KEY,
    email NVARCHAR(200) NOT NULL,
    segment NVARCHAR(20) NULL,
    created_at DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME()
);
GO
IF NOT EXISTS (SELECT 1 FROM dbo.customers)
INSERT INTO dbo.customers (email, segment)
SELECT TOP (1000) CONCAT('user', ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), '@example.com'),
       CASE ABS(CHECKSUM(NEWID())) % 3 WHEN 0 THEN 'vip' WHEN 1 THEN 'repeat' ELSE NULL END
FROM sys.all_objects a CROSS JOIN sys.all_objects b;
GO
