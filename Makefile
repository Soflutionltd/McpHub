INSTALL_DIR := $(HOME)/.local/bin
BIN := $(INSTALL_DIR)/McpHub
# Le binaire est copie et signe a cote, puis renomme par-dessus l'ancien : ecraser sur place
# un binaire en cours d'execution fait tuer le daemon par macOS (signature invalide), et
# launchd attend alors ~10 s avant de le relancer.
BIN_NEW := $(BIN).new
LAUNCHD_SERVICE := gui/$(shell id -u)/com.soflution.mcphub
# Signature stable (Developer ID) : macOS garde les autorisations (TCC) d'une version a l'autre.
# Sans certificat, signature ad hoc (macOS redemandera les acces apres chaque mise a jour).
SIGN_ID ?= $(shell security find-identity -v -p codesigning 2>/dev/null | grep -m1 "Developer ID Application" | sed -E 's/.*"(.*)"/\1/')

.PHONY: build install dev test clean

build:
	cargo build --release

install: build
	@mkdir -p $(INSTALL_DIR)
	cp target/release/McpHub $(BIN_NEW)
	@if [ -n "$(SIGN_ID)" ]; then \
		codesign --force --sign "$(SIGN_ID)" --identifier com.soflution.mcphub --options runtime --timestamp $(BIN_NEW); \
	else \
		codesign --force --sign - --identifier com.soflution.mcphub $(BIN_NEW); \
	fi
	mv -f $(BIN_NEW) $(BIN)
	@pkill -9 -f 'McpHub stdio' 2>/dev/null || true
	@if launchctl print $(LAUNCHD_SERVICE) >/dev/null 2>&1; then \
		launchctl kickstart -k $(LAUNCHD_SERVICE); \
	else \
		pkill -9 -f 'McpHub serve' 2>/dev/null || true; \
	fi
	@echo ""
	@echo "✓ Installed: $(BIN)"
	@$(BIN) version

dev: install
	@echo "Reload Cursor to pick up changes."

test:
	cargo test --release

clean:
	cargo clean
