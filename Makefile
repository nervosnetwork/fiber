CLIPPY_OPTIONS := -- -D warnings -D clippy::redundant_clone -D clippy::enum_glob_use \
	-D clippy::fallible_impl_from -A clippy::mutable_key_type -A clippy::upper_case_acronyms \
	-A clippy::needless_return -A clippy::fallible_impl_from

clippy:
	cargo clippy ${CLIPPY_OPTIONS}

bless-clippy:
	cargo clippy --fix --allow-dirty --allow-staged ${CLIPPY_OPTIONS}