-- Demo data for the local MariaDB: many tables and rows, to try lists, filters and paging.
-- Every value derives from its row number, so each fresh database holds the same data.
-- Keep it in step with demo-postgres.sql. The tests only use `users`, from seed.sql.
-- The row numbers come from the Sequence engine (seq_1_to_N), cast to signed as `n`.

SET NAMES utf8mb4;

-- People and organisation -------------------------------------------------------------------

CREATE TABLE departments (
  id INT PRIMARY KEY,
  name VARCHAR(60) NOT NULL UNIQUE,
  budget DECIMAL(12,2) NOT NULL
);
INSERT INTO departments
SELECT n,
  ELT(n, 'Engineering','Design','Product','Sales','Marketing','Support','Finance','Legal',
         'People','Operations','Security','Data'),
  50000 + n * 12500
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_12) s;

CREATE TABLE employees (
  id INT PRIMARY KEY,
  department_id INT NOT NULL REFERENCES departments(id),
  manager_id INT REFERENCES employees(id),
  full_name VARCHAR(120) NOT NULL,
  email VARCHAR(200) NOT NULL UNIQUE,
  title VARCHAR(80) NOT NULL,
  salary DECIMAL(10,2) NOT NULL,
  hired_on DATE NOT NULL,
  left_on DATE
);
INSERT INTO employees
SELECT n,
  1 + n % 12,
  CASE WHEN n > 12 THEN 1 + n % 12 END,
  CONCAT(
    ELT(1 + n % 20, 'Ada','Alan','Barbara','Claude','Dennis','Edsger','Frances','Grace','Hedy','Ivan',
        'Jean','Ken','Linus','Margaret','Niklaus','Radia','Sophie','Tim','Vint','Yukihiro'),
    ' ',
    ELT(1 + n DIV 20 % 20, 'Allen','Backus','Cerf','Dijkstra','Engelbart','Floyd','Goldberg','Hamilton',
        'Iverson','Jobs','Knuth','Lamport','McCarthy','Naur','Ousterhout','Perlman','Ritchie','Shannon',
        'Thompson','Wirth')),
  CONCAT('employee', n, '@example.com'),
  ELT(1 + n % 8, 'Engineer','Senior Engineer','Staff Engineer','Manager','Director','Analyst',
      'Designer','Specialist'),
  42000 + (n * 613) % 90000,
  DATE '2015-01-01' + INTERVAL (n * 11) % 3600 DAY,
  CASE WHEN n % 9 = 0 THEN DATE '2025-01-01' + INTERVAL n % 300 DAY END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_300) s;

CREATE TABLE customers (
  id INT PRIMARY KEY,
  first_name VARCHAR(60) NOT NULL,
  last_name VARCHAR(60) NOT NULL,
  email VARCHAR(200) NOT NULL UNIQUE,
  phone VARCHAR(30),
  country CHAR(2) NOT NULL,
  tier ENUM('free', 'starter', 'pro', 'enterprise') NOT NULL,
  marketing_opt_in BOOLEAN NOT NULL DEFAULT FALSE,
  lifetime_value DECIMAL(12,2) NOT NULL DEFAULT 0,
  signup_date DATE NOT NULL,
  last_seen_at DATETIME,
  notes TEXT
);
INSERT INTO customers
SELECT n,
  ELT(1 + n % 20, 'Olivia','Liam','Emma','Noah','Ava','Lucas','Mia','Mateo','Sofia','Leo',
      'Isla','Hugo','Nora','Elias','Zoe','Felix','Lena','Oscar','Iris','Milo'),
  ELT(1 + n DIV 20 % 20, 'Smith','Garcia','Müller','Rossi','Dubois','Jansen','Nilsson','Tanaka','Silva',
      'Kowalski','O''Brien','Novak','Andersen','Costa','Fischer','Moreau','Bianchi','Sato','Lopez','Kim'),
  CONCAT('customer', n, '@example.com'),
  CASE WHEN n % 7 <> 0 THEN CONCAT('+1-555-', LPAD(n % 10000, 4, '0')) END,
  ELT(1 + n % 10, 'US','GB','DE','FR','IT','ES','NL','SE','JP','BR'),
  ELT(1 + n % 7, 'free','free','free','starter','starter','pro','enterprise'),
  n % 3 = 0,
  ROUND((n * 37 % 500000) / 100.0, 2),
  DATE '2021-01-01' + INTERVAL n % 1400 DAY,
  CASE WHEN n % 11 <> 0 THEN TIMESTAMP '2025-01-01 00:00:00' + INTERVAL n * 37 MINUTE END,
  CASE WHEN n % 25 = 0 THEN CONCAT('VIP: ', REPEAT('prefers contact by email. ', 1 + n % 8)) END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_10000) s;

CREATE TABLE addresses (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES customers(id),
  kind VARCHAR(10) NOT NULL,
  line1 VARCHAR(120) NOT NULL,
  city VARCHAR(80) NOT NULL,
  postal_code VARCHAR(12) NOT NULL,
  country CHAR(2) NOT NULL,
  is_default BOOLEAN NOT NULL
);
CREATE INDEX addresses_customer_id ON addresses (customer_id);
INSERT INTO addresses
SELECT n,
  1 + (n - 1) % 10000,
  CASE WHEN n <= 10000 THEN 'shipping' ELSE 'billing' END,
  CONCAT(1 + n % 250, ' ',
    ELT(1 + n % 8, 'Main St','Oak Ave','Station Rd','Church Ln','Market Sq','Harbour Way','Mill Rd','Park Blvd')),
  ELT(1 + n % 12, 'Springfield','Riverton','Lakeside','Fairview','Georgetown','Ashford','Milton','Kingsport',
      'Brookfield','Westwood','Northgate','Eastham'),
  LPAD(n * 13 % 100000, 5, '0'),
  ELT(1 + n % 10, 'US','GB','DE','FR','IT','ES','NL','SE','JP','BR'),
  n <= 10000
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_15000) s;

-- Catalogue ---------------------------------------------------------------------------------

CREATE TABLE categories (
  id INT PRIMARY KEY,
  name VARCHAR(60) NOT NULL UNIQUE,
  parent_id INT REFERENCES categories(id)
);
INSERT INTO categories
SELECT n,
  ELT(n, 'Home','Garden','Office','Outdoor','Kitchen','Travel','Lighting','Storage','Furniture',
      'Tools','Textiles','Decor','Cookware','Tableware','Stationery','Bags','Camping','Cycling',
      'Planters','Bedding','Bath','Audio','Toys','Pets'),
  CASE WHEN n > 6 THEN 1 + n % 6 END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_24) s;

CREATE TABLE suppliers (
  id INT PRIMARY KEY,
  name VARCHAR(120) NOT NULL,
  contact_email VARCHAR(200),
  country CHAR(2) NOT NULL,
  rating DECIMAL(2,1) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE
);
INSERT INTO suppliers
SELECT n,
  CONCAT(
    ELT(1 + n % 10, 'Northwind','Acme','Globex','Initech','Umbrella','Hooli','Stark','Wayne','Wonka','Tyrell'),
    ' ', ELT(1 + n DIV 10 % 5, 'Trading','Supply','Industries','Imports','Works'), ' ', n),
  CASE WHEN n % 6 <> 0 THEN CONCAT('sales@supplier', n, '.example.com') END,
  ELT(1 + n % 8, 'CN','DE','US','VN','IT','PL','TR','IN'),
  1 + (n * 7 % 41) / 10.0,
  n % 13 <> 0
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_200) s;

CREATE TABLE products (
  id INT PRIMARY KEY,
  sku VARCHAR(20) NOT NULL UNIQUE,
  name VARCHAR(120) NOT NULL,
  category_id INT NOT NULL REFERENCES categories(id),
  supplier_id INT NOT NULL REFERENCES suppliers(id),
  price DECIMAL(10,2) NOT NULL,
  cost DECIMAL(10,2) NOT NULL,
  stock INT NOT NULL,
  weight_kg FLOAT,
  tags JSON NOT NULL,
  attributes JSON NOT NULL,
  discontinued BOOLEAN NOT NULL DEFAULT FALSE,
  created_at DATETIME NOT NULL
);
INSERT INTO products
SELECT n + 1,
  CONCAT('SKU-', LPAD(n + 1, 6, '0')),
  CONCAT(
    ELT(1 + n % 10, 'Classic','Compact','Deluxe','Eco','Folding','Heavy-Duty','Mini','Modern','Rustic','Smart'),
    ' ',
    ELT(1 + n DIV 10 % 10, 'Oak','Steel','Bamboo','Glass','Linen','Ceramic','Copper','Walnut','Wool','Marble'),
    ' ',
    ELT(1 + n DIV 100 % 10, 'Lamp','Chair','Desk','Shelf','Planter','Kettle','Backpack','Blanket','Clock','Tray')),
  1 + n % 24,
  1 + (n * 7) % 200,
  ROUND(4.99 + (n * 137 % 49500) / 100.0, 2),
  ROUND(2.00 + (n * 137 % 49500) / 250.0, 2),
  CASE WHEN n % 17 = 0 THEN 0 ELSE (n * 53) % 900 END,
  CASE WHEN n % 8 <> 0 THEN 0.1 + (n * 3 % 250) / 10.0 END,
  JSON_ARRAY(ELT(1 + n % 5, 'new','sale','bestseller','limited','gift'), ELT(1 + n % 2, 'indoor','outdoor')),
  JSON_OBJECT(
    'color', ELT(1 + n % 6, 'black','white','red','green','blue','natural'),
    'dimensions', JSON_OBJECT('w', 10 + n % 90, 'h', 5 + n % 60, 'd', 2 + n % 40),
    'warranty_months', 12 * (1 + n % 3)),
  n % 29 = 0,
  TIMESTAMP '2022-01-01 00:00:00' + INTERVAL n * 19 HOUR
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_0_to_999) s;

CREATE TABLE warehouses (
  id INT PRIMARY KEY,
  code CHAR(3) NOT NULL UNIQUE,
  city VARCHAR(80) NOT NULL,
  capacity INT NOT NULL
);
INSERT INTO warehouses
SELECT n,
  ELT(n, 'AMS','BER','CHI','DUB','LIS','NYC','OSA','SAO'),
  ELT(n, 'Amsterdam','Berlin','Chicago','Dublin','Lisbon','New York','Osaka','São Paulo'),
  20000 + n * 7500
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_8) s;

CREATE TABLE inventory_movements (
  id BIGINT PRIMARY KEY,
  product_id INT NOT NULL REFERENCES products(id),
  warehouse_id INT NOT NULL REFERENCES warehouses(id),
  quantity INT NOT NULL,
  reason VARCHAR(20) NOT NULL,
  moved_at DATETIME NOT NULL
);
INSERT INTO inventory_movements
SELECT n,
  1 + (n * 31) % 1000,
  1 + n % 8,
  CASE WHEN n % 3 = 0 THEN -(1 + n % 40) ELSE 1 + n % 120 END,
  ELT(1 + n % 6, 'restock','sale','sale','return','damaged','transfer'),
  TIMESTAMP '2023-01-01 00:00:00' + INTERVAL n * 35 MINUTE
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_30000) s;

-- Sales -------------------------------------------------------------------------------------

CREATE TABLE coupons (
  id INT PRIMARY KEY,
  code VARCHAR(20) NOT NULL UNIQUE,
  percent_off INT NOT NULL,
  valid_until DATE,
  max_uses INT
);
INSERT INTO coupons
SELECT n,
  CONCAT(ELT(1 + n % 5, 'WELCOME','SPRING','SUMMER','AUTUMN','WINTER'), LPAD(n, 2, '0')),
  5 * (1 + n % 8),
  CASE WHEN n % 4 <> 0 THEN DATE '2025-01-01' + INTERVAL n * 9 DAY END,
  CASE WHEN n % 3 = 0 THEN 100 * n END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_50) s;

CREATE TABLE orders (
  id BIGINT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES customers(id),
  status ENUM('pending', 'paid', 'shipped', 'delivered', 'cancelled', 'refunded') NOT NULL,
  currency CHAR(3) NOT NULL,
  total DECIMAL(12,2) NOT NULL,
  coupon_id INT REFERENCES coupons(id),
  placed_at DATETIME NOT NULL,
  shipped_at DATETIME
);
CREATE INDEX orders_customer_id ON orders (customer_id);
CREATE INDEX orders_placed_at ON orders (placed_at);
INSERT INTO orders
SELECT n,
  1 + (n * 7919) % 10000,
  ELT(1 + n % 10, 'delivered','delivered','delivered','delivered','shipped','shipped','paid','pending',
      'cancelled','refunded'),
  ELT(1 + n % 6, 'USD','USD','EUR','EUR','GBP','JPY'),
  ROUND(9.99 + (n * 271 % 250000) / 100.0, 2),
  CASE WHEN n % 9 = 0 THEN 1 + n % 50 END,
  TIMESTAMP '2023-01-01 00:00:00' + INTERVAL n * 21 MINUTE,
  CASE WHEN n % 10 < 6 THEN TIMESTAMP '2023-01-02 06:00:00' + INTERVAL n * 21 MINUTE END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_50000) s;

-- Same shape as orders, and no rows: an empty table to look at.
CREATE TABLE archived_orders LIKE orders;

CREATE TABLE order_items (
  order_id BIGINT NOT NULL REFERENCES orders(id),
  line_no INT NOT NULL,
  product_id INT NOT NULL REFERENCES products(id),
  quantity INT NOT NULL,
  unit_price DECIMAL(10,2) NOT NULL,
  discount DECIMAL(4,2) NOT NULL DEFAULT 0,
  PRIMARY KEY (order_id, line_no)
);
CREATE INDEX order_items_product_id ON order_items (product_id);
INSERT INTO order_items
SELECT 1 + (n - 1) DIV 3,
  1 + (n - 1) % 3,
  1 + (n * 31) % 1000,
  1 + n % 5,
  ROUND(4.99 + (n * 137 % 49500) / 100.0, 2),
  CASE WHEN n % 6 = 0 THEN 0.10 WHEN n % 15 = 0 THEN 0.25 ELSE 0 END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_150000) s;

CREATE TABLE payments (
  id BIGINT PRIMARY KEY,
  order_id BIGINT NOT NULL UNIQUE REFERENCES orders(id),
  method VARCHAR(20) NOT NULL,
  amount DECIMAL(12,2) NOT NULL,
  status VARCHAR(12) NOT NULL,
  provider_ref CHAR(32) NOT NULL,
  paid_at DATETIME
);
INSERT INTO payments
SELECT n,
  n,
  ELT(1 + n % 6, 'card','card','card','paypal','bank_transfer','gift_card'),
  ROUND(9.99 + (n * 271 % 250000) / 100.0, 2),
  CASE WHEN n % 10 = 7 THEN 'pending' WHEN n % 10 = 8 THEN 'failed'
       WHEN n % 10 = 9 THEN 'refunded' ELSE 'captured' END,
  MD5(n),
  CASE WHEN n % 10 NOT IN (7, 8) THEN TIMESTAMP '2023-01-01 00:05:00' + INTERVAL n * 21 MINUTE END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_50000) s;

CREATE TABLE shipments (
  id BIGINT PRIMARY KEY,
  order_id BIGINT NOT NULL REFERENCES orders(id),
  warehouse_id INT NOT NULL REFERENCES warehouses(id),
  carrier VARCHAR(20) NOT NULL,
  tracking_number VARCHAR(30) NOT NULL UNIQUE,
  shipped_at DATETIME NOT NULL,
  delivered_at DATETIME
);
INSERT INTO shipments
SELECT n,
  n,
  1 + n % 8,
  ELT(1 + n % 5, 'DHL','UPS','FedEx','PostNL','Royal Mail'),
  CONCAT('TRK', LPAD(n, 10, '0')),
  TIMESTAMP '2023-01-02 06:00:00' + INTERVAL n * 21 MINUTE,
  CASE WHEN n % 10 < 4 THEN TIMESTAMP '2023-01-05 14:00:00' + INTERVAL n * 21 MINUTE END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_50000) s
WHERE n % 10 < 6;

CREATE TABLE reviews (
  id INT PRIMARY KEY,
  product_id INT NOT NULL REFERENCES products(id),
  customer_id INT NOT NULL REFERENCES customers(id),
  rating SMALLINT NOT NULL CHECK (rating BETWEEN 1 AND 5),
  title VARCHAR(120),
  body TEXT NOT NULL,
  created_at DATETIME NOT NULL
);
INSERT INTO reviews
SELECT n,
  1 + (n * 17) % 1000,
  1 + (n * 113) % 10000,
  CASE WHEN n % 10 < 5 THEN 5 WHEN n % 10 < 8 THEN 4 ELSE 1 + n % 3 END,
  CASE WHEN n % 5 <> 0 THEN
    ELT(1 + n % 6, 'Great value','Does the job','Not what I expected','Would buy again','Arrived late',
        'Better than the photos') END,
  REPEAT(ELT(1 + n % 4, 'Sturdy and well made. ','The colour is slightly off. ','Easy to assemble. ',
             'Packaging was damaged, the item was fine. '), 1 + n % 12),
  TIMESTAMP '2023-03-01 00:00:00' + INTERVAL n * 2 HOUR
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_8000) s;

-- Support and sessions ----------------------------------------------------------------------

CREATE TABLE support_tickets (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES customers(id),
  assignee_id INT REFERENCES employees(id),
  subject VARCHAR(160) NOT NULL,
  priority VARCHAR(10) NOT NULL,
  status VARCHAR(12) NOT NULL,
  opened_at DATETIME NOT NULL,
  closed_at DATETIME
);
INSERT INTO support_tickets
SELECT n,
  1 + (n * 211) % 10000,
  CASE WHEN n % 6 <> 0 THEN 1 + n % 300 END,
  CONCAT(ELT(1 + n % 7, 'Where is my order?','Refund request','Wrong item received','Cannot log in',
             'Change delivery address','Invoice needed','Damaged on arrival'), ' #', n),
  ELT(1 + n % 5, 'low','normal','normal','high','urgent'),
  CASE WHEN n % 4 = 0 THEN 'open' WHEN n % 4 = 1 THEN 'waiting' ELSE 'closed' END,
  TIMESTAMP '2024-01-01 00:00:00' + INTERVAL n * 3 HOUR,
  CASE WHEN n % 4 > 1 THEN TIMESTAMP '2024-01-02 09:00:00' + INTERVAL n * 3 HOUR END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_5000) s;

CREATE TABLE ticket_comments (
  id INT PRIMARY KEY,
  ticket_id INT NOT NULL REFERENCES support_tickets(id),
  author VARCHAR(10) NOT NULL,
  body TEXT NOT NULL,
  created_at DATETIME NOT NULL
);
INSERT INTO ticket_comments
SELECT n,
  1 + (n * 7) % 5000,
  CASE WHEN n % 2 = 0 THEN 'agent' ELSE 'customer' END,
  REPEAT(ELT(1 + n % 4, 'Thanks for getting back to me. ','We are looking into this now. ',
             'Could you send the order number? ','This is now resolved. '), 1 + n % 6),
  TIMESTAMP '2024-01-01 02:00:00' + INTERVAL n * 75 MINUTE
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_12000) s;

CREATE TABLE sessions (
  id UUID PRIMARY KEY,
  customer_id INT REFERENCES customers(id),
  ip INET4 NOT NULL,
  user_agent VARCHAR(200) NOT NULL,
  started_at DATETIME NOT NULL,
  ended_at DATETIME
);
INSERT INTO sessions
SELECT CAST(INSERT(INSERT(MD5(n), 13, 1, '4'), 17, 1, '8') AS UUID),
  CASE WHEN n % 5 <> 0 THEN 1 + (n * 577) % 10000 END,
  CONCAT('10.', n % 256, '.', n DIV 256 % 256, '.', 1 + n % 254),
  ELT(1 + n % 4,
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) Safari/605.1.15',
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126.0 Safari/537.36',
      'Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0',
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) Mobile/15E148'),
  TIMESTAMP '2025-01-01 00:00:00' + INTERVAL n * 13 MINUTE,
  CASE WHEN n % 8 <> 0 THEN TIMESTAMP '2025-01-01 00:25:00' + INTERVAL n * 13 MINUTE END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_20000) s;

-- Small configuration tables ----------------------------------------------------------------

CREATE TABLE feature_flags (
  `key` VARCHAR(60) PRIMARY KEY,
  enabled BOOLEAN NOT NULL,
  rollout_percent INT NOT NULL,
  description TEXT
);
INSERT INTO feature_flags
SELECT CONCAT(ELT(1 + n % 6, 'checkout','search','profile','billing','admin','mobile'), '.',
              ELT(1 + n DIV 6 % 5, 'new_ui','fast_path','beta_api','dark_mode','bulk_edit')),
  n % 3 <> 0,
  (n * 15) % 101,
  CASE WHEN n % 4 <> 0 THEN CONCAT('Rolled out in steps; owner is team ', 1 + n % 12) END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_0_to_29) s;

CREATE TABLE settings (
  `key` VARCHAR(60) PRIMARY KEY,
  value JSON NOT NULL,
  updated_at DATETIME NOT NULL
);
INSERT INTO settings VALUES
  ('site.name', '"Demo Shop"', '2025-01-01 00:00:00'),
  ('site.locales', '["en", "de", "fr", "ja"]', '2025-01-02 00:00:00'),
  ('checkout.guest_allowed', 'true', '2025-01-03 00:00:00'),
  ('checkout.max_items', '50', '2025-01-04 00:00:00'),
  ('shipping.free_over', '{"USD": 50, "EUR": 45, "GBP": 40}', '2025-01-05 00:00:00'),
  ('mail.sender', '{"name": "Demo Shop", "address": "hello@example.com"}', '2025-01-06 00:00:00'),
  ('tax.default_rate', '0.21', '2025-01-07 00:00:00'),
  ('maintenance.window', 'null', '2025-01-08 00:00:00');

-- Tables with an unusual shape ---------------------------------------------------------------

-- A name close to the longest MariaDB allows (64 characters).
CREATE TABLE a_table_with_a_very_long_name_to_see_how_lists_cut_or_wrap_text (
  id INT PRIMARY KEY,
  a_column_with_a_very_long_name_to_see_how_headers_cut_or_wrap VARCHAR(40) NOT NULL
);
INSERT INTO a_table_with_a_very_long_name_to_see_how_lists_cut_or_wrap_text
SELECT seq, CONCAT('row ', seq) FROM seq_1_to_10;

-- 41 columns, to scroll sideways.
SET @columns = (SELECT GROUP_CONCAT(CONCAT('metric_', LPAD(seq, 2, '0'), ' DECIMAL(10,2)')
                                    ORDER BY seq SEPARATOR ', ') FROM seq_1_to_40);
SET @statement = CONCAT('CREATE TABLE wide_metrics (id INT PRIMARY KEY, ', @columns, ')');
PREPARE statement FROM @statement;
EXECUTE statement;
SET @columns = (SELECT GROUP_CONCAT(CONCAT('ROUND(n * ', seq, ' % 9973 / 7.0, 2)')
                                    ORDER BY seq SEPARATOR ', ') FROM seq_1_to_40);
SET @statement = CONCAT('INSERT INTO wide_metrics SELECT n, ', @columns,
                        ' FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_2000) s');
PREPARE statement FROM @statement;
EXECUTE statement;
DEALLOCATE PREPARE statement;

-- One column per kind of value, with NULLs, long text and odd characters.
CREATE TABLE type_samples (
  id INT PRIMARY KEY,
  tiny_number TINYINT,
  small_number SMALLINT,
  big_unsigned BIGINT UNSIGNED,
  exact_number DECIMAL(20,6),
  float_number DOUBLE,
  flag BOOLEAN,
  some_bits BIT(8),
  one_char CHAR(1),
  short_text VARCHAR(40),
  long_text LONGTEXT,
  a_date DATE,
  a_time TIME,
  a_datetime DATETIME(6),
  a_timestamp TIMESTAMP NULL,
  a_year YEAR,
  a_uuid UUID,
  a_json JSON,
  an_enum ENUM('free', 'starter', 'pro', 'enterprise'),
  a_set SET('read', 'write', 'admin'),
  an_inet4 INET4,
  an_inet6 INET6,
  some_bytes VARBINARY(16)
);
INSERT INTO type_samples
SELECT n,
  CASE WHEN n % 10 <> 0 THEN n % 256 - 128 END,
  CASE WHEN n % 9 <> 0 THEN n * 331 % 65535 - 32767 END,
  CASE WHEN n % 8 <> 0 THEN CAST(n AS UNSIGNED) * 92233720368547758 END,
  CASE WHEN n % 7 <> 0 THEN n * 1234567.890123 - 99999999 END,
  CASE WHEN n % 6 <> 0 THEN SQRT(n) * 1e6 / (1 + n % 13) END,
  CASE WHEN n % 5 <> 0 THEN n % 2 = 0 END,
  CASE WHEN n % 23 <> 0 THEN n % 256 END,
  CASE WHEN n % 4 <> 0 THEN CHAR(65 + n % 26 USING utf8mb4) END,
  CASE n % 6 WHEN 0 THEN NULL WHEN 1 THEN '' WHEN 2 THEN '  padded  ' WHEN 3 THEN 'Zoë — ñandú ✓ 日本語'
             WHEN 4 THEN 'O''Reilly "quoted"' ELSE CONCAT('plain ', n) END,
  CASE WHEN n % 3 = 0 THEN REPEAT('Lorem ipsum dolor sit amet, consectetur adipiscing elit. ', n)
       WHEN n % 3 = 1 THEN 'line one\nline two\n\ttabbed line three' END,
  CASE WHEN n % 10 <> 0 THEN DATE '1970-01-01' + INTERVAL n * 97 DAY END,
  CASE WHEN n % 11 <> 0 THEN SEC_TO_TIME(n * 431) END,
  CASE WHEN n % 12 <> 0 THEN TIMESTAMP '2000-01-01 00:00:00.123456' + INTERVAL n * 1000003 SECOND END,
  CASE WHEN n % 13 <> 0 THEN TIMESTAMP '2000-01-01 00:00:00' + INTERVAL n * 1000003 SECOND END,
  CASE WHEN n % 24 <> 0 THEN 1950 + n % 100 END,
  CASE WHEN n % 15 <> 0 THEN CAST(INSERT(INSERT(MD5(n), 13, 1, '4'), 17, 1, '8') AS UUID) END,
  CASE WHEN n % 17 <> 0 THEN JSON_OBJECT('id', n, 'tags', JSON_ARRAY('a', 'b', n),
                                         'nested', JSON_OBJECT('deep', JSON_OBJECT('value', n * 1.5))) END,
  CASE WHEN n % 22 <> 0 THEN ELT(1 + n % 4, 'free','starter','pro','enterprise') END,
  CASE WHEN n % 19 <> 0 THEN ELT(1 + n % 5, '', 'read', 'read,write', 'read,write,admin', 'admin') END,
  CASE WHEN n % 20 <> 0 THEN CONCAT('192.168.', n % 256, '.', 1 + n % 254) END,
  CASE WHEN n % 18 <> 0 THEN CONCAT('2001:db8::', HEX(n)) END,
  CASE WHEN n % 21 <> 0 THEN UNHEX(MD5(n)) END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_200) s;

-- Views -------------------------------------------------------------------------------------

CREATE VIEW active_customers AS
SELECT id, first_name, last_name, email, country, tier, last_seen_at
FROM customers
WHERE last_seen_at >= TIMESTAMP '2025-06-01 00:00:00';

CREATE VIEW order_summaries AS
SELECT o.id, o.placed_at, o.status, c.email AS customer_email, o.currency, o.total,
       count(i.line_no) AS line_count, sum(i.quantity) AS units
FROM orders o
JOIN customers c ON c.id = o.customer_id
LEFT JOIN order_items i ON i.order_id = o.id
GROUP BY o.id, c.email;

-- Other databases, where PostgreSQL has other schemas ---------------------------------------

CREATE DATABASE analytics;
CREATE DATABASE billing;
CREATE DATABASE audit;
GRANT ALL PRIVILEGES ON analytics.* TO 'dev'@'%', 'test'@'%';
GRANT ALL PRIVILEGES ON billing.* TO 'dev'@'%', 'test'@'%';
GRANT ALL PRIVILEGES ON audit.* TO 'dev'@'%', 'test'@'%';

CREATE TABLE analytics.page_views (
  id BIGINT PRIMARY KEY,
  session_id UUID NOT NULL,
  path VARCHAR(200) NOT NULL,
  referrer VARCHAR(200),
  device VARCHAR(10) NOT NULL,
  duration_ms INT NOT NULL,
  viewed_at DATETIME NOT NULL
);
CREATE INDEX page_views_viewed_at ON analytics.page_views (viewed_at);
INSERT INTO analytics.page_views
SELECT n,
  CAST(INSERT(INSERT(MD5(1 + n DIV 5), 13, 1, '4'), 17, 1, '8') AS UUID),
  ELT(1 + n % 8, '/','/products',CONCAT('/products/', 1 + n % 1000),'/cart','/checkout','/account',
      '/search?q=lamp','/help/shipping'),
  CASE WHEN n % 3 <> 0 THEN
    ELT(1 + n % 4, 'https://www.google.com/','https://duckduckgo.com/','https://news.example.com/deals',
        'https://mail.example.com/') END,
  ELT(1 + n % 6, 'desktop','desktop','mobile','mobile','mobile','tablet'),
  200 + (n * 7919) % 180000,
  TIMESTAMP '2025-01-01 00:00:00' + INTERVAL n * 157 SECOND
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_100000) s;

CREATE TABLE analytics.daily_revenue (
  day DATE NOT NULL,
  currency CHAR(3) NOT NULL,
  orders INT NOT NULL,
  revenue DECIMAL(14,2) NOT NULL,
  PRIMARY KEY (day, currency)
);
INSERT INTO analytics.daily_revenue
SELECT DATE '2023-01-01' + INTERVAL d DAY, cur, 20 + (d * 7 + ASCII(cur)) % 60,
  ROUND(1500 + (d * 7919 + ASCII(cur) * 31) % 900000 / 100.0, 2)
FROM (SELECT CAST(seq AS SIGNED) d FROM seq_0_to_729) s,
     (SELECT 'USD' cur UNION ALL SELECT 'EUR' UNION ALL SELECT 'GBP' UNION ALL SELECT 'JPY') c;

CREATE TABLE analytics.funnel_steps (
  funnel VARCHAR(30) NOT NULL,
  step_no INT NOT NULL,
  name VARCHAR(60) NOT NULL,
  visitors INT NOT NULL,
  PRIMARY KEY (funnel, step_no)
);
INSERT INTO analytics.funnel_steps VALUES
  ('checkout', 1, 'Viewed cart', 48210), ('checkout', 2, 'Entered address', 31874),
  ('checkout', 3, 'Chose payment', 27530), ('checkout', 4, 'Placed order', 22961),
  ('signup', 1, 'Opened form', 15400), ('signup', 2, 'Submitted email', 9120),
  ('signup', 3, 'Confirmed email', 7385);

CREATE TABLE billing.plans (
  id INT PRIMARY KEY,
  name VARCHAR(40) NOT NULL UNIQUE,
  monthly_price DECIMAL(8,2) NOT NULL,
  seats INT
);
INSERT INTO billing.plans VALUES
  (1, 'Free', 0, 1), (2, 'Starter', 19, 5), (3, 'Pro', 79, 25), (4, 'Enterprise', 499, NULL);

CREATE TABLE billing.subscriptions (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES app.customers(id),
  plan_id INT NOT NULL REFERENCES billing.plans(id),
  status VARCHAR(12) NOT NULL,
  started_on DATE NOT NULL,
  cancelled_on DATE
);
INSERT INTO billing.subscriptions
SELECT n,
  1 + (n * 3) % 10000,
  1 + n % 4,
  CASE WHEN n % 7 = 0 THEN 'cancelled' WHEN n % 11 = 0 THEN 'past_due' ELSE 'active' END,
  DATE '2022-01-01' + INTERVAL n % 1000 DAY,
  CASE WHEN n % 7 = 0 THEN DATE '2024-10-01' + INTERVAL n % 300 DAY END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_3000) s;

CREATE TABLE billing.invoices (
  id INT PRIMARY KEY,
  subscription_id INT NOT NULL REFERENCES billing.subscriptions(id),
  number VARCHAR(20) NOT NULL UNIQUE,
  amount_due DECIMAL(10,2) NOT NULL,
  paid BOOLEAN NOT NULL,
  issued_on DATE NOT NULL,
  due_on DATE NOT NULL
);
INSERT INTO billing.invoices
SELECT n,
  1 + (n - 1) % 3000,
  CONCAT('INV-', 2024 + (n - 1) DIV 6000, '-', LPAD(n, 6, '0')),
  ELT(1 + (1 + (n - 1) % 3000) % 4, 0, 19, 79, 499),
  n % 12 <> 0,
  DATE '2024-01-01' + INTERVAL ((n - 1) DIV 3000) * 30 DAY,
  DATE '2024-01-15' + INTERVAL ((n - 1) DIV 3000) * 30 DAY
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_12000) s;

CREATE TABLE billing.invoice_lines (
  invoice_id INT NOT NULL REFERENCES billing.invoices(id),
  line_no INT NOT NULL,
  description VARCHAR(120) NOT NULL,
  amount DECIMAL(10,2) NOT NULL,
  PRIMARY KEY (invoice_id, line_no)
);
INSERT INTO billing.invoice_lines
SELECT 1 + (n - 1) DIV 2,
  1 + (n - 1) % 2,
  CASE WHEN n % 2 = 1 THEN 'Plan fee' ELSE 'Extra seats' END,
  CASE WHEN n % 2 = 1 THEN ELT(1 + n % 4, 0, 19, 79, 499) ELSE 5 * (n % 9) END
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_24000) s;

CREATE TABLE audit.change_log (
  id BIGINT PRIMARY KEY,
  table_name VARCHAR(60) NOT NULL,
  row_id BIGINT NOT NULL,
  action VARCHAR(6) NOT NULL,
  changed_by VARCHAR(200) NOT NULL,
  changes JSON,
  changed_at DATETIME NOT NULL
);
INSERT INTO audit.change_log
SELECT n,
  ELT(1 + n % 5, 'customers','orders','products','payments','support_tickets'),
  1 + (n * 37) % 10000,
  ELT(1 + n % 5, 'INSERT','UPDATE','UPDATE','UPDATE','DELETE'),
  CONCAT('employee', 1 + n % 300, '@example.com'),
  CASE WHEN n % 5 BETWEEN 1 AND 3 THEN
    JSON_OBJECT('field', ELT(1 + n % 4, 'status','email','price','notes'),
                'old', CONCAT('value ', n), 'new', CONCAT('value ', n + 1)) END,
  TIMESTAMP '2024-06-01 00:00:00' + INTERVAL n * 11 MINUTE
FROM (SELECT CAST(seq AS SIGNED) n FROM seq_1_to_20000) s;
