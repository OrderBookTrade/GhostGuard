.PHONY: build install uninstall test fmt clippy clean release

# Default install location (same as ghostguardup)
INSTALL_DIR ?= $(HOME)/.ghostguard/bin

build:
	cargo build --release

install: build
	@mkdir -p $(INSTALL_DIR)
	@cp target/release/ghostguard $(INSTALL_DIR)/ghostguard
	@chmod +x $(INSTALL_DIR)/ghostguard
	@echo "installed: $(INSTALL_DIR)/ghostguard"
	@echo ""
	@echo "Make sure $(INSTALL_DIR) is in your PATH:"
	@echo '  export PATH="$$HOME/.ghostguard/bin:$$PATH"'

uninstall:
	@rm -f $(INSTALL_DIR)/ghostguard
	@echo "removed: $(INSTALL_DIR)/ghostguard"

test:
	cargo test

test-live:
	cargo test -- --ignored

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

clean:
	cargo clean

# Create a release tarball for the current platform
release: build
	@mkdir -p dist
	@cd target/release && tar -czf ../../dist/ghostguard-$(shell uname -s | tr '[:upper:]' '[:lower:]')-$(shell uname -m | sed 's/x86_64/amd64/' | sed 's/aarch64/arm64/').tar.gz ghostguard
	@echo "tarball: dist/"
	@ls -lh dist/

# Quick verify with known ghost fill tx
verify-ghost:
	cargo run --release -- --verify-tx 0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0
