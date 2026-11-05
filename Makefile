.PHONY: all web build release dist test lint dev-daemon fmt clean

all: release

web:
	cd web && pnpm install && pnpm build

build:
	cargo build

# Production binaries with the dashboard embedded.
release: web
	cargo build --release --features ferry-daemon/embed-web

# dist/ferry-<version>-<host>.tar.gz, the same layout as the GitHub release assets.
dist: release
	scripts/dist.sh host

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

# Run a daemon with no isolation and fake GPUs for hacking on the CLI/dashboard/MCP.
dev-daemon:
	mkdir -p /tmp/ferry-dev && printf 'listen = "127.0.0.1:7070"\ndata_dir = "/tmp/ferry-dev"\nruntime = "process"\ngpu_provider = "fake:2"\nreserve_cpus = 0\nreserve_mem = "0"\n' > /tmp/ferry-dev/ferry.toml
	cargo run --bin ferryd -- --config /tmp/ferry-dev/ferry.toml

clean:
	cargo clean
	rm -rf web/dist web/node_modules
