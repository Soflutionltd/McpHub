INSTALL_DIR := $(HOME)/.local/bin
BIN := $(INSTALL_DIR)/McpHub

.PHONY: build install dev test clean

build:
	cargo build --release

install: build
	@mkdir -p $(INSTALL_DIR)
	@pkill -9 -f 'McpHub stdio' 2>/dev/null || true
	@pkill -9 -f 'McpHub serve' 2>/dev/null || true
	@sleep 0.3
	cp target/release/McpHub $(BIN)
	codesign --force --sign - $(BIN)
	@echo ""
	@echo "✓ Installed: $(BIN)"
	@$(BIN) help | head -2

dev: install
	@echo "Reload Cursor to pick up changes."

test:
	cargo test --release

clean:
	cargo clean
