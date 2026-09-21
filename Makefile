BIN_DIR ?= $(HOME)/.claude/bin
RELEASE := $(CURDIR)/target/release
BINARIES := inkling slackmcp

.PHONY: help build test check fmt lint install uninstall clean

help:
	@echo "make build      build both binaries in release mode"
	@echo "make check      fmt + clippy + tests, the pre-push gate"
	@echo "make install    build, then copy into $(BIN_DIR)"
	@echo "make uninstall  remove them"

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

# Copy rather than symlink, so a release build on a branch doesn't go live
# behind every session's hooks. Copy-then-rename: overwriting a running
# binary in place gets it SIGKILLed on macOS (its code signature no longer
# matches the pages already mapped).
install: build
	@mkdir -p $(BIN_DIR)
	@for bin in $(BINARIES); do \
		cp $(RELEASE)/$$bin $(BIN_DIR)/.$$bin.tmp && \
		mv -f $(BIN_DIR)/.$$bin.tmp $(BIN_DIR)/$$bin && \
		echo "installed $(BIN_DIR)/$$bin"; \
	done

uninstall:
	@for bin in $(BINARIES); do \
		rm -f $(BIN_DIR)/$$bin && echo "removed $(BIN_DIR)/$$bin"; \
	done

clean:
	cargo clean
