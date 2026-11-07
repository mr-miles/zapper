# The Office: firms of AI workers in sandboxes of an OpenShell gateway.
#
#   make build             zapper-office, zapper-workbench, zapper-inspect and the static guest `office` command (release)
#   make install           zapper-office, zapper-workbench and zapper-inspect into ~/.local/bin
#   make image             a sandbox image from a recipe in config/zapper.yaml (IMAGE=office by default)
#   make image IMAGE=office-fake  a fake Claude Code and pi, for the office's e2e tests
#   make test              unit tests (no gateway)
#   make lint              rustfmt and clippy (warnings are errors), as the pre-commit hook runs them
#   make fmt               format the code with rustfmt
#   make hooks             use the repository's git hooks (.githooks: pre-commit runs make lint)
#   make clean
#
# Long targets print to the terminal; to follow them elsewhere:
#   make image > /tmp/zapper-image.log 2>&1 &  tail -f /tmp/zapper-image.log

MUSL    := x86_64-unknown-linux-musl
TARGET  := target/release
AGENT   := target/$(MUSL)/release/zapper-agent
PREFIX  ?= $(HOME)/.local
IMAGE   ?= office
HOST    := -p zapper-office -p zapper-inspect -p zapper-keeper -p zapper-workbench
BINS    := zapper-office zapper-inspect zapper-workbench
# The fake Claude Code and pi of the office's e2e tests, static like the
# guest command: the office-fake image installs it.
FAKE    := -p zapper-fake

.PHONY: build install image test lint fmt hooks clean

build:
	cargo build --release -p zapper-agent $(FAKE) --target $(MUSL)
	cargo build --release $(HOST)
	cp $(AGENT) $(TARGET)/zapper-agent

install: build
	install -d $(PREFIX)/bin
	install -m 0755 $(addprefix $(TARGET)/,$(BINS)) $(PREFIX)/bin/

image: build
	$(TARGET)/zapper-office image $(IMAGE) --agent $(TARGET)/zapper-agent

test:
	cargo test --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --release -- -D warnings

fmt:
	cargo fmt --all

hooks:
	git config core.hooksPath .githooks

clean:
	cargo clean
