-- Demo data for the local PostgreSQL: many tables and rows, to try lists, filters and paging.
-- Every value derives from its row number, so each fresh database holds the same data.
-- Keep it in step with demo-mariadb.sql. The tests only use `users`, from seed.sql.

CREATE TYPE customer_tier AS ENUM ('free', 'starter', 'pro', 'enterprise');
CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'delivered', 'cancelled', 'refunded');

-- People and organisation -------------------------------------------------------------------

CREATE TABLE departments (
  id INT PRIMARY KEY,
  name VARCHAR(60) NOT NULL UNIQUE,
  budget NUMERIC(12,2) NOT NULL
);
INSERT INTO departments
SELECT n,
  (ARRAY['Engineering','Design','Product','Sales','Marketing','Support','Finance','Legal',
         'People','Operations','Security','Data'])[n],
  50000 + n * 12500
FROM generate_series(1, 12) n;

CREATE TABLE employees (
  id INT PRIMARY KEY,
  department_id INT NOT NULL REFERENCES departments(id),
  manager_id INT REFERENCES employees(id),
  full_name VARCHAR(120) NOT NULL,
  email VARCHAR(200) NOT NULL UNIQUE,
  title VARCHAR(80) NOT NULL,
  salary NUMERIC(10,2) NOT NULL,
  hired_on DATE NOT NULL,
  left_on DATE
);
INSERT INTO employees
SELECT n,
  1 + n % 12,
  CASE WHEN n > 12 THEN 1 + n % 12 END,
  (ARRAY['Ada','Alan','Barbara','Claude','Dennis','Edsger','Frances','Grace','Hedy','Ivan',
         'Jean','Ken','Linus','Margaret','Niklaus','Radia','Sophie','Tim','Vint','Yukihiro'])[1 + n % 20]
    || ' ' ||
  (ARRAY['Allen','Backus','Cerf','Dijkstra','Engelbart','Floyd','Goldberg','Hamilton','Iverson',
         'Jobs','Knuth','Lamport','McCarthy','Naur','Ousterhout','Perlman','Ritchie','Shannon',
         'Thompson','Wirth'])[1 + (n / 20) % 20],
  'employee' || n || '@example.com',
  (ARRAY['Engineer','Senior Engineer','Staff Engineer','Manager','Director','Analyst',
         'Designer','Specialist'])[1 + n % 8],
  42000 + (n * 613) % 90000,
  DATE '2015-01-01' + (n * 11) % 3600,
  CASE WHEN n % 9 = 0 THEN DATE '2025-01-01' + n % 300 END
FROM generate_series(1, 300) n;

CREATE TABLE customers (
  id INT PRIMARY KEY,
  first_name VARCHAR(60) NOT NULL,
  last_name VARCHAR(60) NOT NULL,
  email VARCHAR(200) NOT NULL UNIQUE,
  phone VARCHAR(30),
  country CHAR(2) NOT NULL,
  tier customer_tier NOT NULL,
  marketing_opt_in BOOLEAN NOT NULL DEFAULT FALSE,
  lifetime_value NUMERIC(12,2) NOT NULL DEFAULT 0,
  signup_date DATE NOT NULL,
  last_seen_at TIMESTAMPTZ,
  notes TEXT
);
INSERT INTO customers
SELECT n,
  (ARRAY['Olivia','Liam','Emma','Noah','Ava','Lucas','Mia','Mateo','Sofia','Leo',
         'Isla','Hugo','Nora','Elias','Zoe','Felix','Lena','Oscar','Iris','Milo'])[1 + n % 20],
  (ARRAY['Smith','Garcia','Müller','Rossi','Dubois','Jansen','Nilsson','Tanaka','Silva','Kowalski',
         'O''Brien','Novak','Andersen','Costa','Fischer','Moreau','Bianchi','Sato','Lopez','Kim'])[1 + (n / 20) % 20],
  'customer' || n || '@example.com',
  CASE WHEN n % 7 <> 0 THEN '+1-555-' || lpad((n % 10000)::text, 4, '0') END,
  (ARRAY['US','GB','DE','FR','IT','ES','NL','SE','JP','BR'])[1 + n % 10],
  (ARRAY['free','free','free','starter','starter','pro','enterprise'])[1 + n % 7]::customer_tier,
  n % 3 = 0,
  round((n * 37 % 500000) / 100.0, 2),
  DATE '2021-01-01' + n % 1400,
  CASE WHEN n % 11 <> 0 THEN TIMESTAMPTZ '2025-01-01 00:00:00+00' + n * INTERVAL '37 minutes' END,
  CASE WHEN n % 25 = 0 THEN 'VIP: ' || repeat('prefers contact by email. ', 1 + n % 8) END
FROM generate_series(1, 10000) n;

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
  (1 + n % 250) || ' ' ||
    (ARRAY['Main St','Oak Ave','Station Rd','Church Ln','Market Sq','Harbour Way','Mill Rd','Park Blvd'])[1 + n % 8],
  (ARRAY['Springfield','Riverton','Lakeside','Fairview','Georgetown','Ashford','Milton','Kingsport',
         'Brookfield','Westwood','Northgate','Eastham'])[1 + n % 12],
  lpad((n * 13 % 100000)::text, 5, '0'),
  (ARRAY['US','GB','DE','FR','IT','ES','NL','SE','JP','BR'])[1 + n % 10],
  n <= 10000
FROM generate_series(1, 15000) n;

-- Catalogue ---------------------------------------------------------------------------------

CREATE TABLE categories (
  id INT PRIMARY KEY,
  name VARCHAR(60) NOT NULL UNIQUE,
  parent_id INT REFERENCES categories(id)
);
INSERT INTO categories
SELECT n,
  (ARRAY['Home','Garden','Office','Outdoor','Kitchen','Travel','Lighting','Storage','Furniture',
         'Tools','Textiles','Decor','Cookware','Tableware','Stationery','Bags','Camping','Cycling',
         'Planters','Bedding','Bath','Audio','Toys','Pets'])[n],
  CASE WHEN n > 6 THEN 1 + n % 6 END
FROM generate_series(1, 24) n;

CREATE TABLE suppliers (
  id INT PRIMARY KEY,
  name VARCHAR(120) NOT NULL,
  contact_email VARCHAR(200),
  country CHAR(2) NOT NULL,
  rating NUMERIC(2,1) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE
);
INSERT INTO suppliers
SELECT n,
  (ARRAY['Northwind','Acme','Globex','Initech','Umbrella','Hooli','Stark','Wayne','Wonka','Tyrell'])[1 + n % 10]
    || ' ' || (ARRAY['Trading','Supply','Industries','Imports','Works'])[1 + (n / 10) % 5] || ' ' || n,
  CASE WHEN n % 6 <> 0 THEN 'sales@supplier' || n || '.example.com' END,
  (ARRAY['CN','DE','US','VN','IT','PL','TR','IN'])[1 + n % 8],
  1 + (n * 7 % 41) / 10.0,
  n % 13 <> 0
FROM generate_series(1, 200) n;

CREATE TABLE products (
  id INT PRIMARY KEY,
  sku VARCHAR(20) NOT NULL UNIQUE,
  name VARCHAR(120) NOT NULL,
  category_id INT NOT NULL REFERENCES categories(id),
  supplier_id INT NOT NULL REFERENCES suppliers(id),
  price NUMERIC(10,2) NOT NULL,
  cost NUMERIC(10,2) NOT NULL,
  stock INT NOT NULL,
  weight_kg REAL,
  tags TEXT[] NOT NULL,
  attributes JSONB NOT NULL,
  discontinued BOOLEAN NOT NULL DEFAULT FALSE,
  created_at TIMESTAMPTZ NOT NULL
);
INSERT INTO products
SELECT n + 1,
  'SKU-' || lpad((n + 1)::text, 6, '0'),
  (ARRAY['Classic','Compact','Deluxe','Eco','Folding','Heavy-Duty','Mini','Modern','Rustic','Smart'])[1 + n % 10]
    || ' ' ||
  (ARRAY['Oak','Steel','Bamboo','Glass','Linen','Ceramic','Copper','Walnut','Wool','Marble'])[1 + (n / 10) % 10]
    || ' ' ||
  (ARRAY['Lamp','Chair','Desk','Shelf','Planter','Kettle','Backpack','Blanket','Clock','Tray'])[1 + (n / 100) % 10],
  1 + n % 24,
  1 + (n * 7) % 200,
  round(4.99 + (n * 137 % 49500) / 100.0, 2),
  round(2.00 + (n * 137 % 49500) / 250.0, 2),
  CASE WHEN n % 17 = 0 THEN 0 ELSE (n * 53) % 900 END,
  CASE WHEN n % 8 <> 0 THEN (0.1 + (n * 3 % 250) / 10.0)::real END,
  ARRAY[(ARRAY['new','sale','bestseller','limited','gift'])[1 + n % 5],
        (ARRAY['indoor','outdoor'])[1 + n % 2]],
  jsonb_build_object(
    'color', (ARRAY['black','white','red','green','blue','natural'])[1 + n % 6],
    'dimensions', jsonb_build_object('w', 10 + n % 90, 'h', 5 + n % 60, 'd', 2 + n % 40),
    'warranty_months', 12 * (1 + n % 3)),
  n % 29 = 0,
  TIMESTAMPTZ '2022-01-01 00:00:00+00' + n * INTERVAL '19 hours'
FROM generate_series(0, 999) n;

CREATE TABLE warehouses (
  id INT PRIMARY KEY,
  code CHAR(3) NOT NULL UNIQUE,
  city VARCHAR(80) NOT NULL,
  capacity INT NOT NULL
);
INSERT INTO warehouses
SELECT n,
  (ARRAY['AMS','BER','CHI','DUB','LIS','NYC','OSA','SAO'])[n],
  (ARRAY['Amsterdam','Berlin','Chicago','Dublin','Lisbon','New York','Osaka','São Paulo'])[n],
  20000 + n * 7500
FROM generate_series(1, 8) n;

CREATE TABLE inventory_movements (
  id BIGINT PRIMARY KEY,
  product_id INT NOT NULL REFERENCES products(id),
  warehouse_id INT NOT NULL REFERENCES warehouses(id),
  quantity INT NOT NULL,
  reason VARCHAR(20) NOT NULL,
  moved_at TIMESTAMPTZ NOT NULL
);
INSERT INTO inventory_movements
SELECT n,
  1 + (n * 31) % 1000,
  1 + n % 8,
  CASE WHEN n % 3 = 0 THEN -(1 + n % 40) ELSE 1 + n % 120 END,
  (ARRAY['restock','sale','sale','return','damaged','transfer'])[1 + n % 6],
  TIMESTAMPTZ '2023-01-01 00:00:00+00' + n * INTERVAL '35 minutes'
FROM generate_series(1, 30000) n;

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
  (ARRAY['WELCOME','SPRING','SUMMER','AUTUMN','WINTER'])[1 + n % 5] || lpad(n::text, 2, '0'),
  5 * (1 + n % 8),
  CASE WHEN n % 4 <> 0 THEN DATE '2025-01-01' + n * 9 END,
  CASE WHEN n % 3 = 0 THEN 100 * n END
FROM generate_series(1, 50) n;

CREATE TABLE orders (
  id BIGINT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES customers(id),
  status order_status NOT NULL,
  currency CHAR(3) NOT NULL,
  total NUMERIC(12,2) NOT NULL,
  coupon_id INT REFERENCES coupons(id),
  placed_at TIMESTAMPTZ NOT NULL,
  shipped_at TIMESTAMPTZ
);
CREATE INDEX orders_customer_id ON orders (customer_id);
CREATE INDEX orders_placed_at ON orders (placed_at);
INSERT INTO orders
SELECT n,
  1 + (n * 7919) % 10000,
  (ARRAY['delivered','delivered','delivered','delivered','shipped','shipped','paid','pending',
         'cancelled','refunded'])[1 + n % 10]::order_status,
  (ARRAY['USD','USD','EUR','EUR','GBP','JPY'])[1 + n % 6],
  round(9.99 + (n * 271 % 250000) / 100.0, 2),
  CASE WHEN n % 9 = 0 THEN 1 + n % 50 END,
  TIMESTAMPTZ '2023-01-01 00:00:00+00' + n * INTERVAL '21 minutes',
  CASE WHEN n % 10 < 6 THEN TIMESTAMPTZ '2023-01-02 06:00:00+00' + n * INTERVAL '21 minutes' END
FROM generate_series(1, 50000) n;

-- Same shape as orders, and no rows: an empty table to look at.
CREATE TABLE archived_orders (LIKE orders INCLUDING ALL);

CREATE TABLE order_items (
  order_id BIGINT NOT NULL REFERENCES orders(id),
  line_no INT NOT NULL,
  product_id INT NOT NULL REFERENCES products(id),
  quantity INT NOT NULL,
  unit_price NUMERIC(10,2) NOT NULL,
  discount NUMERIC(4,2) NOT NULL DEFAULT 0,
  PRIMARY KEY (order_id, line_no)
);
CREATE INDEX order_items_product_id ON order_items (product_id);
INSERT INTO order_items
SELECT 1 + (n - 1) / 3,
  1 + (n - 1) % 3,
  1 + (n * 31) % 1000,
  1 + n % 5,
  round(4.99 + (n * 137 % 49500) / 100.0, 2),
  CASE WHEN n % 6 = 0 THEN 0.10 WHEN n % 15 = 0 THEN 0.25 ELSE 0 END
FROM generate_series(1, 150000) n;

CREATE TABLE payments (
  id BIGINT PRIMARY KEY,
  order_id BIGINT NOT NULL UNIQUE REFERENCES orders(id),
  method VARCHAR(20) NOT NULL,
  amount NUMERIC(12,2) NOT NULL,
  status VARCHAR(12) NOT NULL,
  provider_ref CHAR(32) NOT NULL,
  paid_at TIMESTAMPTZ
);
INSERT INTO payments
SELECT n,
  n,
  (ARRAY['card','card','card','paypal','bank_transfer','gift_card'])[1 + n % 6],
  round(9.99 + (n * 271 % 250000) / 100.0, 2),
  CASE WHEN n % 10 = 7 THEN 'pending' WHEN n % 10 = 8 THEN 'failed'
       WHEN n % 10 = 9 THEN 'refunded' ELSE 'captured' END,
  md5(n::text),
  CASE WHEN n % 10 NOT IN (7, 8) THEN TIMESTAMPTZ '2023-01-01 00:05:00+00' + n * INTERVAL '21 minutes' END
FROM generate_series(1, 50000) n;

CREATE TABLE shipments (
  id BIGINT PRIMARY KEY,
  order_id BIGINT NOT NULL REFERENCES orders(id),
  warehouse_id INT NOT NULL REFERENCES warehouses(id),
  carrier VARCHAR(20) NOT NULL,
  tracking_number VARCHAR(30) NOT NULL UNIQUE,
  shipped_at TIMESTAMPTZ NOT NULL,
  delivered_at TIMESTAMPTZ
);
INSERT INTO shipments
SELECT n,
  n,
  1 + n % 8,
  (ARRAY['DHL','UPS','FedEx','PostNL','Royal Mail'])[1 + n % 5],
  'TRK' || lpad(n::text, 10, '0'),
  TIMESTAMPTZ '2023-01-02 06:00:00+00' + n * INTERVAL '21 minutes',
  CASE WHEN n % 10 < 4 THEN TIMESTAMPTZ '2023-01-05 14:00:00+00' + n * INTERVAL '21 minutes' END
FROM generate_series(1, 50000) n
WHERE n % 10 < 6;

CREATE TABLE reviews (
  id INT PRIMARY KEY,
  product_id INT NOT NULL REFERENCES products(id),
  customer_id INT NOT NULL REFERENCES customers(id),
  rating SMALLINT NOT NULL CHECK (rating BETWEEN 1 AND 5),
  title VARCHAR(120),
  body TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);
INSERT INTO reviews
SELECT n,
  1 + (n * 17) % 1000,
  1 + (n * 113) % 10000,
  CASE WHEN n % 10 < 5 THEN 5 WHEN n % 10 < 8 THEN 4 ELSE 1 + n % 3 END,
  CASE WHEN n % 5 <> 0 THEN
    (ARRAY['Great value','Does the job','Not what I expected','Would buy again','Arrived late',
           'Better than the photos'])[1 + n % 6] END,
  repeat((ARRAY['Sturdy and well made. ','The colour is slightly off. ','Easy to assemble. ',
                'Packaging was damaged, the item was fine. '])[1 + n % 4], 1 + n % 12),
  TIMESTAMPTZ '2023-03-01 00:00:00+00' + n * INTERVAL '2 hours'
FROM generate_series(1, 8000) n;

-- Support and sessions ----------------------------------------------------------------------

CREATE TABLE support_tickets (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES customers(id),
  assignee_id INT REFERENCES employees(id),
  subject VARCHAR(160) NOT NULL,
  priority VARCHAR(10) NOT NULL,
  status VARCHAR(12) NOT NULL,
  opened_at TIMESTAMPTZ NOT NULL,
  closed_at TIMESTAMPTZ
);
INSERT INTO support_tickets
SELECT n,
  1 + (n * 211) % 10000,
  CASE WHEN n % 6 <> 0 THEN 1 + n % 300 END,
  (ARRAY['Where is my order?','Refund request','Wrong item received','Cannot log in',
         'Change delivery address','Invoice needed','Damaged on arrival'])[1 + n % 7] || ' #' || n,
  (ARRAY['low','normal','normal','high','urgent'])[1 + n % 5],
  CASE WHEN n % 4 = 0 THEN 'open' WHEN n % 4 = 1 THEN 'waiting' ELSE 'closed' END,
  TIMESTAMPTZ '2024-01-01 00:00:00+00' + n * INTERVAL '3 hours',
  CASE WHEN n % 4 > 1 THEN TIMESTAMPTZ '2024-01-02 09:00:00+00' + n * INTERVAL '3 hours' END
FROM generate_series(1, 5000) n;

CREATE TABLE ticket_comments (
  id INT PRIMARY KEY,
  ticket_id INT NOT NULL REFERENCES support_tickets(id),
  author VARCHAR(10) NOT NULL,
  body TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);
INSERT INTO ticket_comments
SELECT n,
  1 + (n * 7) % 5000,
  CASE WHEN n % 2 = 0 THEN 'agent' ELSE 'customer' END,
  repeat((ARRAY['Thanks for getting back to me. ','We are looking into this now. ',
                'Could you send the order number? ','This is now resolved. '])[1 + n % 4], 1 + n % 6),
  TIMESTAMPTZ '2024-01-01 02:00:00+00' + n * INTERVAL '75 minutes'
FROM generate_series(1, 12000) n;

CREATE TABLE sessions (
  id UUID PRIMARY KEY,
  customer_id INT REFERENCES customers(id),
  ip INET NOT NULL,
  user_agent VARCHAR(200) NOT NULL,
  started_at TIMESTAMPTZ NOT NULL,
  ended_at TIMESTAMPTZ
);
INSERT INTO sessions
SELECT md5(n::text)::uuid,
  CASE WHEN n % 5 <> 0 THEN 1 + (n * 577) % 10000 END,
  ('10.' || n % 256 || '.' || (n / 256) % 256 || '.' || 1 + n % 254)::inet,
  (ARRAY['Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) Safari/605.1.15',
         'Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126.0 Safari/537.36',
         'Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0',
         'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) Mobile/15E148'])[1 + n % 4],
  TIMESTAMPTZ '2025-01-01 00:00:00+00' + n * INTERVAL '13 minutes',
  CASE WHEN n % 8 <> 0 THEN TIMESTAMPTZ '2025-01-01 00:25:00+00' + n * INTERVAL '13 minutes' END
FROM generate_series(1, 20000) n;

-- Small configuration tables ----------------------------------------------------------------

CREATE TABLE feature_flags (
  key VARCHAR(60) PRIMARY KEY,
  enabled BOOLEAN NOT NULL,
  rollout_percent INT NOT NULL,
  description TEXT
);
INSERT INTO feature_flags
SELECT (ARRAY['checkout','search','profile','billing','admin','mobile'])[1 + n % 6] || '.' ||
       (ARRAY['new_ui','fast_path','beta_api','dark_mode','bulk_edit'])[1 + (n / 6) % 5],
  n % 3 <> 0,
  (n * 15) % 101,
  CASE WHEN n % 4 <> 0 THEN 'Rolled out in steps; owner is team ' || 1 + n % 12 END
FROM generate_series(0, 29) n;

CREATE TABLE settings (
  key VARCHAR(60) PRIMARY KEY,
  value JSONB NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
INSERT INTO settings VALUES
  ('site.name', '"Demo Shop"', '2025-01-01 00:00:00+00'),
  ('site.locales', '["en", "de", "fr", "ja"]', '2025-01-02 00:00:00+00'),
  ('checkout.guest_allowed', 'true', '2025-01-03 00:00:00+00'),
  ('checkout.max_items', '50', '2025-01-04 00:00:00+00'),
  ('shipping.free_over', '{"USD": 50, "EUR": 45, "GBP": 40}', '2025-01-05 00:00:00+00'),
  ('mail.sender', '{"name": "Demo Shop", "address": "hello@example.com"}', '2025-01-06 00:00:00+00'),
  ('tax.default_rate', '0.21', '2025-01-07 00:00:00+00'),
  ('maintenance.window', 'null', '2025-01-08 00:00:00+00');

-- Tables with an unusual shape ---------------------------------------------------------------

-- A name as long as PostgreSQL allows (63 characters).
CREATE TABLE a_table_with_a_very_long_name_to_see_how_lists_cut_or_wrap_text (
  id INT PRIMARY KEY,
  a_column_with_a_very_long_name_to_see_how_headers_cut_or_wrap VARCHAR(40) NOT NULL
);
INSERT INTO a_table_with_a_very_long_name_to_see_how_lists_cut_or_wrap_text
SELECT n, 'row ' || n FROM generate_series(1, 10) n;

-- 41 columns, to scroll sideways.
DO $$
BEGIN
  EXECUTE format('CREATE TABLE wide_metrics (id INT PRIMARY KEY, %s)',
    (SELECT string_agg(format('metric_%s NUMERIC(10,2)', lpad(c::text, 2, '0')), ', ' ORDER BY c)
     FROM generate_series(1, 40) c));
  EXECUTE format('INSERT INTO wide_metrics SELECT n, %s FROM generate_series(1, 2000) n',
    (SELECT string_agg(format('round(n * %s %% 9973 / 7.0, 2)', c), ', ' ORDER BY c)
     FROM generate_series(1, 40) c));
END $$;

-- One column per kind of value, with NULLs, long text and odd characters.
CREATE TABLE type_samples (
  id INT PRIMARY KEY,
  small_number SMALLINT,
  big_number BIGINT,
  exact_number NUMERIC(20,6),
  float_number DOUBLE PRECISION,
  flag BOOLEAN,
  one_char CHAR(1),
  short_text VARCHAR(40),
  long_text TEXT,
  a_date DATE,
  a_time TIME,
  a_timestamp TIMESTAMP,
  a_timestamptz TIMESTAMPTZ,
  an_interval INTERVAL,
  a_uuid UUID,
  a_json JSON,
  a_jsonb JSONB,
  an_int_array INT[],
  a_text_array TEXT[],
  an_inet INET,
  some_bytes BYTEA,
  a_tier customer_tier
);
INSERT INTO type_samples
SELECT n,
  CASE WHEN n % 9 <> 0 THEN (n * 331 % 65535 - 32767)::smallint END,
  CASE WHEN n % 8 <> 0 THEN n::bigint * 46116860184273879 END,
  CASE WHEN n % 7 <> 0 THEN n * 1234567.890123 - 99999999 END,
  CASE WHEN n % 6 <> 0 THEN sqrt(n) * 1e6 / (1 + n % 13) END,
  CASE WHEN n % 5 <> 0 THEN n % 2 = 0 END,
  CASE WHEN n % 4 <> 0 THEN chr(65 + n % 26) END,
  CASE n % 6 WHEN 0 THEN NULL WHEN 1 THEN '' WHEN 2 THEN '  padded  ' WHEN 3 THEN 'Zoë — ñandú ✓ 日本語'
             WHEN 4 THEN 'O''Reilly "quoted"' ELSE 'plain ' || n END,
  CASE WHEN n % 3 = 0 THEN repeat('Lorem ipsum dolor sit amet, consectetur adipiscing elit. ', n)
       WHEN n % 3 = 1 THEN E'line one\nline two\n\ttabbed line three' END,
  CASE WHEN n % 10 <> 0 THEN DATE '1970-01-01' + n * 97 END,
  CASE WHEN n % 11 <> 0 THEN TIME '00:00:00' + n * INTERVAL '431 seconds' END,
  CASE WHEN n % 12 <> 0 THEN TIMESTAMP '2000-01-01 00:00:00' + n * INTERVAL '1000003 seconds' END,
  CASE WHEN n % 13 <> 0 THEN TIMESTAMPTZ '2000-01-01 00:00:00+00' + n * INTERVAL '1000003.123456 seconds' END,
  CASE WHEN n % 14 <> 0 THEN n * INTERVAL '90 minutes' END,
  CASE WHEN n % 15 <> 0 THEN md5(n::text)::uuid END,
  CASE WHEN n % 16 <> 0 THEN json_build_object('n', n, 'even', n % 2 = 0) END,
  CASE WHEN n % 17 <> 0 THEN jsonb_build_object('id', n, 'tags', jsonb_build_array('a', 'b', n),
                                                'nested', jsonb_build_object('deep', jsonb_build_object('value', n * 1.5))) END,
  CASE WHEN n % 18 <> 0 THEN ARRAY[n, n * 2, n * 3] END,
  CASE WHEN n % 19 <> 0 THEN ARRAY['one', 'two words', NULL, 'with,comma'] END,
  CASE WHEN n % 20 <> 0 THEN ('192.168.' || n % 256 || '.' || 1 + n % 254)::inet END,
  CASE WHEN n % 21 <> 0 THEN decode(md5(n::text), 'hex') END,
  CASE WHEN n % 22 <> 0 THEN (ARRAY['free','starter','pro','enterprise'])[1 + n % 4]::customer_tier END
FROM generate_series(1, 200) n;

-- Views -------------------------------------------------------------------------------------

CREATE VIEW active_customers AS
SELECT id, first_name, last_name, email, country, tier, last_seen_at
FROM customers
WHERE last_seen_at >= TIMESTAMPTZ '2025-06-01 00:00:00+00';

CREATE VIEW order_summaries AS
SELECT o.id, o.placed_at, o.status, c.email AS customer_email, o.currency, o.total,
       count(i.line_no) AS line_count, sum(i.quantity) AS units
FROM orders o
JOIN customers c ON c.id = o.customer_id
LEFT JOIN order_items i ON i.order_id = o.id
GROUP BY o.id, c.email;

-- Other schemas -----------------------------------------------------------------------------

CREATE SCHEMA analytics;

CREATE TABLE analytics.page_views (
  id BIGINT PRIMARY KEY,
  session_id UUID NOT NULL,
  path VARCHAR(200) NOT NULL,
  referrer VARCHAR(200),
  device VARCHAR(10) NOT NULL,
  duration_ms INT NOT NULL,
  viewed_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX page_views_viewed_at ON analytics.page_views (viewed_at);
INSERT INTO analytics.page_views
SELECT n,
  md5((1 + n / 5)::text)::uuid,
  (ARRAY['/','/products','/products/' || 1 + n % 1000,'/cart','/checkout','/account','/search?q=lamp',
         '/help/shipping'])[1 + n % 8],
  CASE WHEN n % 3 <> 0 THEN
    (ARRAY['https://www.google.com/','https://duckduckgo.com/','https://news.example.com/deals',
           'https://mail.example.com/'])[1 + n % 4] END,
  (ARRAY['desktop','desktop','mobile','mobile','mobile','tablet'])[1 + n % 6],
  200 + (n * 7919) % 180000,
  TIMESTAMPTZ '2025-01-01 00:00:00+00' + n * INTERVAL '157 seconds'
FROM generate_series(1, 100000) n;

CREATE TABLE analytics.daily_revenue (
  day DATE NOT NULL,
  currency CHAR(3) NOT NULL,
  orders INT NOT NULL,
  revenue NUMERIC(14,2) NOT NULL,
  PRIMARY KEY (day, currency)
);
INSERT INTO analytics.daily_revenue
SELECT DATE '2023-01-01' + d, cur, 20 + (d * 7 + ascii(cur)) % 60,
  round(1500 + (d * 7919 + ascii(cur) * 31) % 900000 / 100.0, 2)
FROM generate_series(0, 729) d, unnest(ARRAY['USD','EUR','GBP','JPY']) cur;

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

CREATE SCHEMA billing;

CREATE TABLE billing.plans (
  id INT PRIMARY KEY,
  name VARCHAR(40) NOT NULL UNIQUE,
  monthly_price NUMERIC(8,2) NOT NULL,
  seats INT
);
INSERT INTO billing.plans VALUES
  (1, 'Free', 0, 1), (2, 'Starter', 19, 5), (3, 'Pro', 79, 25), (4, 'Enterprise', 499, NULL);

CREATE TABLE billing.subscriptions (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES public.customers(id),
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
  DATE '2022-01-01' + n % 1000,
  CASE WHEN n % 7 = 0 THEN DATE '2024-10-01' + n % 300 END
FROM generate_series(1, 3000) n;

CREATE TABLE billing.invoices (
  id INT PRIMARY KEY,
  subscription_id INT NOT NULL REFERENCES billing.subscriptions(id),
  number VARCHAR(20) NOT NULL UNIQUE,
  amount_due NUMERIC(10,2) NOT NULL,
  paid BOOLEAN NOT NULL,
  issued_on DATE NOT NULL,
  due_on DATE NOT NULL
);
INSERT INTO billing.invoices
SELECT n,
  1 + (n - 1) % 3000,
  'INV-' || (2024 + (n - 1) / 6000) || '-' || lpad(n::text, 6, '0'),
  (ARRAY[0, 19, 79, 499])[1 + (1 + (n - 1) % 3000) % 4],
  n % 12 <> 0,
  DATE '2024-01-01' + ((n - 1) / 3000) * 30,
  DATE '2024-01-15' + ((n - 1) / 3000) * 30
FROM generate_series(1, 12000) n;

CREATE TABLE billing.invoice_lines (
  invoice_id INT NOT NULL REFERENCES billing.invoices(id),
  line_no INT NOT NULL,
  description VARCHAR(120) NOT NULL,
  amount NUMERIC(10,2) NOT NULL,
  PRIMARY KEY (invoice_id, line_no)
);
INSERT INTO billing.invoice_lines
SELECT 1 + (n - 1) / 2,
  1 + (n - 1) % 2,
  CASE WHEN n % 2 = 1 THEN 'Plan fee' ELSE 'Extra seats' END,
  CASE WHEN n % 2 = 1 THEN (ARRAY[0, 19, 79, 499])[1 + n % 4] ELSE 5 * (n % 9) END
FROM generate_series(1, 24000) n;

CREATE SCHEMA audit;

CREATE TABLE audit.change_log (
  id BIGINT PRIMARY KEY,
  table_name VARCHAR(60) NOT NULL,
  row_id BIGINT NOT NULL,
  action VARCHAR(6) NOT NULL,
  changed_by VARCHAR(200) NOT NULL,
  changes JSONB,
  changed_at TIMESTAMPTZ NOT NULL
);
INSERT INTO audit.change_log
SELECT n,
  (ARRAY['customers','orders','products','payments','support_tickets'])[1 + n % 5],
  1 + (n * 37) % 10000,
  (ARRAY['INSERT','UPDATE','UPDATE','UPDATE','DELETE'])[1 + n % 5],
  'employee' || 1 + n % 300 || '@example.com',
  CASE WHEN n % 5 BETWEEN 1 AND 3 THEN
    jsonb_build_object('field', (ARRAY['status','email','price','notes'])[1 + n % 4],
                       'old', 'value ' || n, 'new', 'value ' || n + 1) END,
  TIMESTAMPTZ '2024-06-01 00:00:00+00' + n * INTERVAL '11 minutes'
FROM generate_series(1, 20000) n;
