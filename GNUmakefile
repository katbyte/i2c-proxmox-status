BIN     := i2c-proxmox-status
PREFIX  ?= /usr/local
SERVICE := packaging/$(BIN).service

default: build

build: ## release build for the panel
	cargo build --release

sim: ## preview in an SDL window (needs SDL2)
	cargo run --features simulator -- --simulator

test:
	cargo test

fmt: ## reformat the code
	cargo fmt

lint: ## what CI enforces: formatting + clippy with warnings as errors
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

install: build ## install the binary and systemd service, start at boot (run as root on the Pi)
	install -m755 target/release/$(BIN) $(PREFIX)/bin/$(BIN)
	install -m644 $(SERVICE) /etc/systemd/system/$(BIN).service
	systemctl daemon-reload
	systemctl enable --now $(BIN).service

uninstall:
	-systemctl disable --now $(BIN).service
	rm -f /etc/systemd/system/$(BIN).service $(PREFIX)/bin/$(BIN)
	systemctl daemon-reload

.PHONY: default build sim test fmt lint install uninstall
