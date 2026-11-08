-- Portable seed data: runs on both PostgreSQL and MariaDB.
CREATE TABLE users (
  id INT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  email VARCHAR(200),
  active BOOLEAN NOT NULL DEFAULT TRUE,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO users (id, name, email, active) VALUES
  (1, 'Ada Lovelace', 'ada@example.com', TRUE),
  (2, 'Edgar Codd', 'codd@example.com', TRUE),
  (3, 'Grace Hopper', NULL, FALSE);
