# zapper: policy-bound Firecracker sandboxes.
#
#   make deps              Firecracker and a guest kernel into ~/.local/share/zapper
#   make build             zapper, zapper-inspect and the static guest agent (release)
#   make install           zapper, zapper-agent and zapper-inspect into ~/.local/bin
#   make image             the base image (IMAGE=ubuntu-26.04; recipes are `images:` in config/zapper.yaml)
#   make image IMAGE=office  office workers: Claude Code and pi (pinned), git, kubectl, podman, a headless browser
#   make image IMAGE=k3s     the cluster facility (pinned k3s, air-gap images)
#   make image IMAGE=office-fake  a fake Claude Code and pi, for the office's e2e tests
#   make kernel KERNEL=k8s   a guest kernel with Kubernetes support
#   make check             report missing prerequisites with the fix; changes nothing
#   make test              unit tests (no VMs)
#   make lint              rustfmt and clippy (warnings are errors), as the pre-commit hook runs them
#   make fmt               format the code with rustfmt
#   make hooks             use the repository's git hooks (.githooks: pre-commit runs make lint)
#   OFFICE=0               with any of the above: zapper without The Office (no firms)
#   make test-e2e          boots real VMs: policy, secrets, exec, attacks (needs KVM, network)
#   make clean
#
# Long targets print to the terminal; to follow them elsewhere:
#   make test-e2e > /tmp/zapper-e2e.log 2>&1 &  tail -f /tmp/zapper-e2e.log

MUSL    := x86_64-unknown-linux-musl
TARGET  := target/release
AGENT   := target/$(MUSL)/release/zapper-agent
PREFIX  ?= $(HOME)/.local
IMAGE   ?= ubuntu-26.04
ZAPPER  := $(TARGET)/zapper
KERNEL  ?= k8s
# OFFICE=0 builds zapper without The Office (cargo --no-default-features),
# and without zapper-inspect, which reads firms' runs; lint and tests leave
# out the office's crates too.
OFFICE  ?= 1
ifeq ($(OFFICE),0)
FEATURES := --no-default-features
HOST     := -p zapper -p zapper-keeper
BINS     := zapper zapper-agent
EXCLUDE  := --exclude zapper-office --exclude zapper-inspect --exclude zapper-runs --exclude zapper-fake
FAKE     :=
else
FEATURES :=
HOST     := -p zapper -p zapper-keeper -p zapper-inspect
BINS     := zapper zapper-agent zapper-inspect
EXCLUDE  :=
# The fake Claude Code and pi of the office's e2e tests, static like the
# agent: the office-fake image installs it.
FAKE     := -p zapper-fake
endif

# Image recipes are in the config (config/zapper.yaml, `images:`).

.PHONY: deps build install image kernel check test lint fmt hooks test-e2e test-e2e-office clean

deps:
	scripts/fetch-deps.sh

build:
	cargo build --release $(FEATURES) -p zapper-agent $(FAKE) --target $(MUSL)
	cargo build --release $(FEATURES) $(HOST)
	cp $(AGENT) $(TARGET)/zapper-agent

install: build
	install -d $(PREFIX)/bin
	install -m 0755 $(addprefix $(TARGET)/,$(BINS)) $(PREFIX)/bin/

image: build
	$(ZAPPER) image build --local $(IMAGE)

kernel:
	scripts/build-kernel.sh $(KERNEL)

check: build
	$(ZAPPER) check

test:
	cargo test --workspace $(EXCLUDE) $(FEATURES)

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace $(EXCLUDE) --all-targets --release $(FEATURES) -- -D warnings
	# The fuzz targets' entry points (feature `fuzz`), which no other build has.
	cargo clippy -p zapper-core --features fuzz --all-targets --release -- -D warnings

fmt:
	cargo fmt --all

hooks:
	git config core.hooksPath .githooks

# The office's tests play the fake and scripts of this checkout: their image
# is rebuilt first (quick when only they changed).
test-e2e: build
	tests/e2e.sh $(ZAPPER)
	$(ZAPPER) image build --local office-fake
	tests/e2e-office.sh $(ZAPPER)

test-e2e-office: build
	$(ZAPPER) image build --local office-fake
	tests/e2e-office.sh $(ZAPPER)

clean:
	cargo clean
