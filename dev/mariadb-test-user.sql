-- Passwordless account for the app's "Local test databases" folder.
CREATE USER 'test'@'%' IDENTIFIED BY '';
GRANT ALL PRIVILEGES ON app.* TO 'test'@'%';
