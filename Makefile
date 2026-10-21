INSTALL_DIR := $(HOME)/.local/bin
BIN := $(INSTALL_DIR)/McpHub
# Signature stable (Developer ID) : macOS garde les autorisations (TCC) d'une version a l'autre.
# Sans certificat, signature ad hoc (macOS redemandera les acces apres chaque mise a jour).
SIGN_ID ?= $(shell security find-identity -v -p codesigning 2>/dev/null | grep -m1 "Developer ID Application" | sed -E 's/.*"(.*)"/\1/')

.PHONY: build install dev test clean

build:
	cargo build --release

install: build
	@mkdir -p $(INSTALL_DIR)
	@pkill -9 -f 'McpHub stdio' 2>/dev/null || true
	@pkill -9 -f 'McpHub serve' 2>/dev/null || true
	@sleep 0.3
	cp target/release/McpHub $(BIN)
	@if [ -n "$(SIGN_ID)" ]; then \
		codesign --force --sign "$(SIGN_ID)" --identifier com.soflution.mcphub --options runtime --timestamp $(BIN); \
	else \
		codesign --force --sign - --identifier com.soflution.mcphub $(BIN); \
	fi
	@echo ""
	@echo "✓ Installed: $(BIN)"
	@$(BIN) help | head -2

dev: install
	@echo "Reload Cursor to pick up changes."

test:
	cargo test --release

clean:
	cargo clean
