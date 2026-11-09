# Development checks. CONTRIBUTING.md explains each one.
.PHONY: build test lint fmt msrv browser audit docs ci demo docker

build:
	cargo build --locked --release

test:
	cargo test --locked --all-targets

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo clippy --locked --all-targets --features browser -- -D warnings
	git diff --check $$(git hash-object -t tree /dev/null)

fmt:
	cargo fmt --all

msrv:
	cargo +1.89.0 test --locked --all-targets
	cargo +1.89.0 check --locked --all-targets --features browser

browser:
	AUDITROO_REQUIRE_BROWSER_TESTS=1 cargo test --locked --all-targets --features browser

audit:
	cargo audit --file Cargo.lock

docs:
	AUDITROO_UPDATE_DOCS=1 cargo test --locked --test docs

ci: lint test msrv audit

demo:
	cargo run --locked --quiet -- demo

docker:
	docker build -t auditroo:dev .
