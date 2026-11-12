# Shortcuts for local development. See docs/DEVELOPMENT.md.

# The dev app keeps its own config, so it never touches the installed app's.
export TUPLEBASE_CONFIG_DIR ?= $(CURDIR)/target/dev-config

# The dev app's AWS is the fake one from docker-compose.yml, so the test-mariadb-aws
# connection works with no account. `make dev REAL_AWS=1` uses your real AWS instead.
ifndef REAL_AWS
export AWS_ENDPOINT_URL := http://127.0.0.1:54566
export AWS_ACCESS_KEY_ID := test
export AWS_SECRET_ACCESS_KEY := test
unexport AWS_PROFILE AWS_SESSION_TOKEN
endif

.PHONY: dev up down

# Starts the local databases, adds them to the dev config and runs the app.
dev: up
	cargo run -p tuplebase-core --example add_test_connections
	cargo run -p tuplebase-app

up:
	docker compose up -d --wait

down:
	docker compose down
