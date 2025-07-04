CARGO_TARGET_DIR ?= target
COVERAGE_PROFRAW_DIR ?= ${CARGO_TARGET_DIR}/coverage
GRCOV_OUTPUT ?= coverage-report.info
GRCOV_EXCL_START = ^\s*((log::|tracing::)?(trace|debug|info|warn|error)|(debug_)?assert(_eq|_ne|_error_eq))!\($$
GRCOV_EXCL_STOP  = ^\s*\)(;)?$$
GRCOV_EXCL_LINE = ^\s*(\})*(\))*(;)*$$|\s*((log::|tracing::)?(trace|debug|info|warn|error)|(debug_)?assert(_eq|_ne|_error_eq))!\(.*\)(;)?$$

.PHONY: test
test:
	RUST_LOG=off cargo nextest run --no-fail-fast

.PHONY: check
check:
	cargo check
	cargo check --release
	cargo check --package fnn --no-default-features
	cd migrate && cargo check

.PHONY: clippy
clippy:
	cargo clippy --all --all-targets --all-features -- -D warnings -A clippy::module-inception

.PHONY: bless
bless:
	cargo clippy --fix --allow-dirty --allow-staged --all --all-targets --all-features

.PHONY: fmt
fmt:
	cargo fmt --all -- --check

coverage-clean:
	rm -rf "${CARGO_TARGET_DIR}/*.profraw" "${GRCOV_OUTPUT}" "${GRCOV_OUTPUT:.info=}"

coverage-install-tools:
	rustup component add llvm-tools-preview
	grcov --version || cargo install --locked grcov

coverage-run-unittests:
	mkdir -p "${COVERAGE_PROFRAW_DIR}"
	rm -f "${COVERAGE_PROFRAW_DIR}/*.profraw"
	RUSTFLAGS="${RUSTFLAGS} -Cinstrument-coverage" \
		RUST_LOG=off \
		LLVM_PROFILE_FILE="${COVERAGE_PROFRAW_DIR}/unittests-%p-%m.profraw" \
			cargo test --all

coverage-collect-data:
	grcov "${COVERAGE_PROFRAW_DIR}" --binary-path "${CARGO_TARGET_DIR}/debug/" \
		-s . -t lcov --branch --ignore-not-existing \
		--ignore "/*" \
		--ignore "*/tests/*" \
		--ignore "*/tests.rs" \
		--excl-br-start "${GRCOV_EXCL_START}" --excl-br-stop "${GRCOV_EXCL_STOP}" \
		--excl-start    "${GRCOV_EXCL_START}" --excl-stop    "${GRCOV_EXCL_STOP}" \
		--excl-br-line  "${GRCOV_EXCL_LINE}" \
		--excl-line     "${GRCOV_EXCL_LINE}" \
		-o "${GRCOV_OUTPUT}"

coverage-generate-report:
	genhtml --ignore-errors inconsistent --ignore-errors corrupt --ignore-errors range --ignore-errors unmapped -o "${GRCOV_OUTPUT:.info=}" "${GRCOV_OUTPUT}"

coverage: coverage-run-unittests coverage-collect-data coverage-generate-report

RPC_GEN_VERSION = 0.1.11
.PHONY: gen-rpc-doc
gen-rpc-doc:
	@if ! command -v fiber-rpc-gen >/dev/null 2>&1 || [ "$$(fiber-rpc-gen --version | awk '{print $$2}')" != "$(RPC_GEN_VERSION)" ]; then \
        echo "Installing fiber-rpc-gen $(RPC_GEN_VERSION)..."; \
        cargo install fiber-rpc-gen --version $(RPC_GEN_VERSION) --force; \
	fi
	fiber-rpc-gen ./crates/fiber-lib/src/
	if grep -q "TODO: add desc" ./crates/fiber-lib/src/rpc/README.md; then \
        echo "Warning: There are 'TODO: add desc' in src/rpc/README.md, please add documentation comments to resolve them"; \
		exit 1; \
    fi

.PHONY: check-dirty-rpc-doc
check-dirty-rpc-doc: gen-rpc-doc
	git diff --exit-code ./crates/fiber-lib/src/rpc/README.md

MIGRATION_CHECK_VERSION := 0.2.4
install-migration-check:
	@if ! command -v migration-check >/dev/null 2>&1 || [ "$$(migration-check --version | awk '{print $$2}')" != "$(MIGRATION_CHECK_VERSION)" ]; then \
		echo "Installing migration-check $(MIGRATION_CHECK_VERSION)..."; \
		cargo install migration-check --version $(MIGRATION_CHECK_VERSION) --force; \
	fi

.PHONY: check-migrate
check-migrate: install-migration-check
	migration-check -s ./crates/fiber-lib/src -o ./crates/fiber-lib/src/store/.schema.json

.PHONY: update-migrate-check
update-migrate-check: install-migration-check
	migration-check -s ./crates/fiber-lib/src -o ./crates/fiber-lib/src/store/.schema.json -u

