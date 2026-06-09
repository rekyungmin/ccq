# Dev convenience wrappers. The committed universal `bin/ccq` is the plugin's
# shipped artifact, so it must track source — `build-universal` rebuilds it and
# `install-hooks` wires a pre-push freshness check.
.PHONY: build-universal check-bin-fresh install-hooks test lint fmt

build-universal: ## Rebuild the committed universal (arm64+x86_64) bin/ccq
	sh scripts/build-universal.sh

check-bin-fresh: ## Fail if bin/ccq is stale vs source (what the pre-push hook runs)
	sh scripts/check-bin-fresh.sh

install-hooks: ## Enable the versioned git hooks (pre-push bin/ccq freshness)
	git config core.hooksPath .githooks
	@echo "git hooks enabled (core.hooksPath=.githooks)"

test: ## cargo test
	cargo test

lint: ## rustfmt check + clippy (warnings = errors) — what the pre-commit hook runs
	sh scripts/check-lint.sh

fmt: ## apply rustfmt
	cargo fmt
