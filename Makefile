
.PHONY: clippy
clippy:
	cargo clippy --all --all-targets --all-features

.PHONY: bless
bless:
	cargo clippy --fix --allow-dirty --allow-staged --all --all-targets --all-features

.PHONY: fmt
fmt:
	cargo fmt --all -- --check