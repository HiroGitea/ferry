.PHONY: all web build release test dev-daemon fmt clean

all: release

web:
	cd web && pnpm install && pnpm build

build:
	cargo build

# Production binaries with the dashboard embedded.
release: web
	cargo build --release --features mlq-daemon/embed-web

test:
	cargo test --workspace

fmt:
	cargo fmt --all

# Run a daemon with no isolation and fake GPUs for hacking on the CLI/dashboard/MCP.
dev-daemon:
	mkdir -p /tmp/mlq-dev && printf 'listen = "127.0.0.1:7070"\ndata_dir = "/tmp/mlq-dev"\nruntime = "process"\ngpu_provider = "fake:2"\nreserve_cpus = 0\nreserve_mem = "0"\n' > /tmp/mlq-dev/mlq.toml
	cargo run --bin mlqd -- --config /tmp/mlq-dev/mlq.toml

clean:
	cargo clean
	rm -rf web/dist web/node_modules
