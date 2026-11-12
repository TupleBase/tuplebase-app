# Shortcuts for local development. See docs/DEVELOPMENT.md.

# The dev app keeps its own config, so it never touches the installed app's.
export TUPLEBASE_CONFIG_DIR ?= $(CURDIR)/target/dev-config

.PHONY: dev dev-fake-aws up down

# Starts the local databases, adds them to the dev config and runs the app.
dev: up
	cargo run -p tuplebase-core --example add_test_connections
	cargo run -p tuplebase-app

# The same, with AWS pointed at the fake service from docker-compose.yml.
dev-fake-aws:
	env -u AWS_PROFILE AWS_ENDPOINT_URL=http://127.0.0.1:54566 \
		AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test $(MAKE) dev

up:
	docker compose up -d --wait

down:
	docker compose down
