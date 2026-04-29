# pilot — reactive PR inbox TUI

LIBGHOSTTY_DIR := ../libghostty-rs
LIBGHOSTTY_REPO := https://github.com/ghostty-org/libghostty-rs.git

.PHONY: all setup build run test clean help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

all: setup build ## Setup dependencies and build

setup: ## Install dependencies (libghostty-rs, zig, gh)
	@echo "Checking dependencies..."
	@command -v cargo >/dev/null || { echo "Error: cargo not found. Install Rust: https://rustup.rs"; exit 1; }
	@command -v gh >/dev/null || { echo "Error: gh not found. Install: brew install gh (macOS) or https://cli.github.com"; exit 1; }
	@command -v zig >/dev/null || { echo "Error: zig not found. Install: brew install zig (macOS) or https://ziglang.org/download/"; exit 1; }
	@command -v tmux >/dev/null || { echo "Warning: tmux not found. Sessions won't persist across quit. Install: brew install tmux"; }
	@if [ ! -d "$(LIBGHOSTTY_DIR)" ]; then \
		echo "Cloning libghostty-rs..."; \
		git clone $(LIBGHOSTTY_REPO) $(LIBGHOSTTY_DIR); \
	else \
		echo "libghostty-rs found at $(LIBGHOSTTY_DIR)"; \
	fi
	@echo "All dependencies OK."

build: ## Build pilot (debug)
	cargo build -p pilot-v2-tui

release: ## Build pilot (optimized)
	cargo build -p pilot-v2-tui --release

run: ## Build and run pilot
	cargo run -p pilot-v2-tui

test: ## Run all tests
	cargo test --workspace

lint: ## Run clippy
	cargo clippy --workspace

clean: ## Clean build artifacts
	cargo clean

install: release ## Install to ~/.cargo/bin
	cp target/release/pilot ~/.cargo/bin/pilot
	@echo "Installed to ~/.cargo/bin/pilot"
