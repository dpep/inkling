BIN_DIR ?= $(HOME)/.claude/bin
RELEASE := $(CURDIR)/target/release
BINARIES := inkling slackmcp

.PHONY: help build test check fmt lint install uninstall clean

help:
	@echo "make build      build both binaries in release mode"
	@echo "make check      fmt + clippy + tests, the pre-push gate"
	@echo "make install    build, then symlink into $(BIN_DIR)"
	@echo "make uninstall  remove those symlinks"

build:
	cargo build --release

test:
	cargo test

fmt:
	cargo fmt --check

lint:
	cargo clippy --all-targets -- -D warnings

# Everything that must pass before pushing.
check: test fmt lint

# Symlink rather than copy, so a rebuild is live immediately — the same
# pattern as the other tools in ~/.claude/bin.
install: build
	@mkdir -p $(BIN_DIR)
	@for bin in $(BINARIES); do \
		ln -sf $(RELEASE)/$$bin $(BIN_DIR)/$$bin; \
		echo "$(BIN_DIR)/$$bin -> $(RELEASE)/$$bin"; \
	done

uninstall:
	@for bin in $(BINARIES); do \
		rm -f $(BIN_DIR)/$$bin && echo "removed $(BIN_DIR)/$$bin"; \
	done

clean:
	cargo clean
