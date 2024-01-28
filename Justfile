build:
	cargo -Z build-std build --target=x86_64-unknown-linux-gnu

build_release:
	cargo -Z build-std build --target=x86_64-unknown-linux-gnu --release

run: build
	./target/x86_64-unknown-linux-gnu/debug/mcstatus-http

check:
	cargo clippy --all-targets --all-features
