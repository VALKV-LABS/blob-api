.PHONY: unit integ down

# Pass NO_CACHE=1 to bust Docker layer cache: make unit NO_CACHE=1
BUILD_ARGS := $(if $(NO_CACHE),--no-cache,)

# ── Unit tests (cargo test in Docker, no external services) ──────────────────
unit:
	docker compose -f docker-compose.test.yml --profile unit build $(BUILD_ARGS) blob-unit-tests
	docker compose -f docker-compose.test.yml --profile unit run --rm blob-unit-tests
	docker compose -f docker-compose.test.yml --profile unit down -v 2>/dev/null || true

# ── Integration tests (Rust binary + S3 + Postgres + Node.js runner) ──────────
# The suite runs twice: standalone on an empty Postgres, and beside an auth
# service on supabase/postgres as the non-superuser supabase_storage_admin.
integ:
	docker compose -f docker-compose.test.yml --profile integ down -v 2>/dev/null || true
	docker compose -f docker-compose.test.yml --profile integ build $(BUILD_ARGS) blob-api-test
	@echo "== standalone (empty Postgres) =="
	docker compose -f docker-compose.test.yml --profile integ run --rm --no-TTY blob-integ-tests && \
	echo "== beside an auth service (supabase/postgres, supabase_storage_admin) ==" && \
	docker compose -f docker-compose.test.yml --profile integ run --rm --no-TTY blob-integ-tests-supabase; \
	rc=$$?; docker compose -f docker-compose.test.yml --profile integ down -v 2>/dev/null || true; exit $$rc

# ── Tear down all test containers and volumes ─────────────────────────────────
down:
	docker compose -f docker-compose.test.yml down -v 2>/dev/null || true
