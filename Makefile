CLIPPY_OPTIONS := -- -D warnings -D clippy::redundant_clone -D clippy::enum_glob_use \
	-D clippy::fallible_impl_from -A clippy::mutable_key_type -A clippy::upper_case_acronyms \
	-A clippy::needless_return -A clippy::fallible_impl_from

.PHONY: clippy
clippy:
	cargo clippy ${CLIPPY_OPTIONS}

.PHONY: fmt
fmt:
	cargo fmt --all -- --check

.PHONY: bless
bless:
	cargo clippy --fix --allow-dirty --allow-staged ${CLIPPY_OPTIONS}