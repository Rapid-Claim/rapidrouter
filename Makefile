# caret-router
#
# `make` on its own lists everything. The targets that talk to real
# providers source `.env` first (the binary has no dotenv of its own), so
# put provider keys there and they apply to `run`, `live` and the SDK
# suite alike.
#
# The `ci-*` targets mirror .github/workflows/ci.yml job for job — if one
# passes here it passes there, and `make ci` runs the lot.

.DEFAULT_GOAL := help
SHELL := /bin/bash

CARGO   ?= cargo
ENV_FILE ?= .env
CONFIG  ?=
PORT    ?=
# Project-local by default, deliberately. The gateway is managed-mode: it
# remembers the last config it was given, so a shared data dir means
# `make run` silently inherits state from whatever you ran days ago.
# Keeping it in-tree (and gitignored) makes a dev run reproducible and
# leaves ~/.caret-router — a real deployment's state — untouched.
DATA_DIR ?= .caret-data
# Overridable so `make live-subs MODEL=…` can pin a model.
MODEL   ?= claude-sonnet-4-5-20250929

# Provider keys live in .env, which is gitignored. Sourced rather than
# `include`d: include chokes on quoted values and would export comments.
# `.` searches PATH for a bare name, so a relative ENV_FILE is anchored to
# the tree explicitly; an absolute one is passed through untouched.
LOAD_ENV = if [ -f "$(ENV_FILE)" ]; then \
             case "$(ENV_FILE)" in /*) _e="$(ENV_FILE)";; *) _e="./$(ENV_FILE)";; esac; \
             set -a; . "$$_e"; set +a; \
           fi

RUN_FLAGS = --data-dir $(DATA_DIR) \
            $(if $(CONFIG),--config $(CONFIG)) \
            $(if $(PORT),--port $(PORT))

.PHONY: help
help: ## List every target
	@awk 'BEGIN {FS = ":.*?## "} \
	     /^## /      {printf "\n\033[1m%s\033[0m\n", substr($$0, 4)} \
	     /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}' \
	     $(MAKEFILE_LIST)
	@echo

## Run
.PHONY: run
run: ## Start the gateway on :8080 with human-readable logs (CONFIG=, PORT=)
	@$(LOAD_ENV); $(CARGO) run -p router-bin -- --dev $(RUN_FLAGS)

.PHONY: run-release
run-release: ## Start the optimized build — use this for anything perf-related
	@$(LOAD_ENV); $(CARGO) run --release -p router-bin -- --dev $(RUN_FLAGS)

.PHONY: check-config
check-config: ## Validate a config and exit: make check-config CONFIG=subs.toml
	@test -n "$(CONFIG)" || { echo "set CONFIG=<file>"; exit 2; }
	@$(LOAD_ENV); $(CARGO) run -q -p router-bin -- check $(CONFIG)

.PHONY: build
build: ## Debug build
	$(CARGO) build --workspace

.PHONY: release
release: ## Optimized build
	$(CARGO) build --release -p router-bin

## Check
.PHONY: test
test: ## Whole workspace test suite
	$(CARGO) test --workspace

.PHONY: fmt
fmt: ## Format in place
	$(CARGO) fmt --all

.PHONY: lint
lint: ## Clippy over every target, warnings denied (as CI does)
	RUSTFLAGS="-D warnings" $(CARGO) clippy --workspace --all-targets

.PHONY: verify
verify: ## fmt check + lint + test — run before pushing
	$(CARGO) fmt --all --check
	RUSTFLAGS="-D warnings" $(CARGO) clippy --workspace --all-targets
	$(CARGO) test --workspace

## CI jobs (mirror .github/workflows/ci.yml)
.PHONY: ci
ci: verify ci-control-plane ci-loom ci-console ci-sdk-suite ci-perf ## Every CI job except audit and the live suites

.PHONY: ci-control-plane
ci-control-plane: ## Store backends, store facade, fleet behaviour
	$(CARGO) test -p router-store --test backends --release
	$(CARGO) test -p router-store --test store --release
	$(CARGO) test -p router-server --test fleet --release

.PHONY: ci-loom
ci-loom: ## Model-check the breaker and token bucket under loom
	RUSTFLAGS="--cfg loom" $(CARGO) test -p router-core --test loom_models --release

.PHONY: ci-sdk-suite
ci-sdk-suite: ## Official OpenAI/Anthropic SDKs against gateway + mock
	@python3 -m venv /tmp/caret-sdkvenv
	@/tmp/caret-sdkvenv/bin/pip install --quiet --upgrade openai anthropic
	scripts/sdk-suite/run.sh /tmp/caret-sdkvenv/bin/python

.PHONY: ci-perf
ci-perf: ## Benches compile, overhead gate, short soak
	$(CARGO) bench --workspace -- --test
	$(CARGO) run --release -p rig -- overhead --rps 200 --secs 5 --assert-p50-us 400 --assert-p99-us 4000
	$(CARGO) run --release -p rig -- soak --secs 20 --rps 100 --assert-rss-growth-pct 40

.PHONY: audit
audit: ## cargo-audit over the dependency tree
	$(CARGO) audit

## Console (console/)
.PHONY: console-dev
console-dev: ## Vite dev server against a gateway you are already running
	cd console && npm run dev

.PHONY: console-build
console-build: ## Typecheck and build the console bundle
	cd console && npm ci && npm run typecheck && npm run build

.PHONY: ci-console
ci-console: console-build ## Console build, e2e, and bundle budget
	cd console && npx playwright install --with-deps chromium && npm run test:e2e && npm run budget
	$(CARGO) build -p router-server --no-default-features

## Live providers (spends real quota)
.PHONY: live
live: ## Conformance shapes against real provider APIs; skips per missing key
	@$(LOAD_ENV); $(CARGO) test -p router-server --test live_validation -- --ignored --nocapture

.PHONY: live-subs
live-subs: ## Subscription seats against real backends (MODEL= to pin one)
	@$(LOAD_ENV); \
	 if [ -z "$$LIVE_CLAUDE_SUBSCRIPTION_TOKEN" ] && [ -z "$$LIVE_CODEX_AUTH_JSON" ]; then \
	   echo "Set LIVE_CLAUDE_SUBSCRIPTION_TOKEN (see 'make setup-token') and/or"; \
	   echo "LIVE_CODEX_AUTH_JSON=\$$HOME/.codex/auth.json, in $(ENV_FILE) or the environment."; \
	   exit 2; \
	 fi; \
	 LIVE_CLAUDE_SUBSCRIPTION_MODEL="$(MODEL)" \
	   $(CARGO) test -p router-server --test live_subscriptions -- --ignored --nocapture

.PHONY: setup-token
setup-token: ## Mint a 1-year Claude subscription token (interactive, opens a browser)
	@echo "Running 'claude setup-token'. Put the printed sk-ant-oat01-… into $(ENV_FILE) as:"
	@echo "  LIVE_CLAUDE_SUBSCRIPTION_TOKEN=sk-ant-oat01-…"
	@echo
	@claude setup-token

## Housekeeping
.PHONY: reset
reset: ## Forget the local gateway state (config, keys, usage) in $(DATA_DIR)
	@rm -rf $(DATA_DIR)
	@echo "cleared $(DATA_DIR)"

.PHONY: clean
clean: ## Remove all build output
	$(CARGO) clean

.PHONY: clean-incremental
clean-incremental: ## Drop the incremental cache only — the usual disk hog
	@du -sh target/debug/incremental 2>/dev/null || true
	@rm -rf target/debug/incremental
	@echo "incremental cache cleared"
