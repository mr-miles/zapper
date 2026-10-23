# zapper: policy-bound Firecracker sandboxes.
#
#   make deps              Firecracker and a guest kernel into ~/.local/share/zapper
#   make build             zapper, zapper-inspect and the static guest agent (release)
#   make install           zapper, zapper-agent and zapper-inspect into ~/.local/bin
#   make image             the base image (IMAGE=ubuntu-26.04, see IMAGES below)
#   make image IMAGE=office  office workers: Claude Code and pi (pinned), git, kubectl, podman, a headless browser
#   make image IMAGE=k3s     the cluster facility (pinned k3s, air-gap images)
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
EXCLUDE  := --exclude zapper-office --exclude zapper-inspect --exclude zapper-runs
else
FEATURES :=
HOST     := -p zapper -p zapper-keeper -p zapper-inspect
BINS     := zapper zapper-agent zapper-inspect
EXCLUDE  :=
endif

# Image recipes: name → base and build steps.
BASE_PKGS := apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git less procps && rm -rf /var/lib/apt/lists/*
IMAGE_FROM_ubuntu-26.04 := docker.io/library/ubuntu:26.04
IMAGE_RUN_ubuntu-26.04  := --run "$(BASE_PKGS)"
# Office workers: Claude Code at a pinned version (auto-update is off in
# sessions), with the tools engineers deploy with. Bump the versions here and
# rebuild to update.
CLAUDE_VERSION := 2.1.301
# A headless browser for workers to check what they build: Playwright and
# its Chromium (the `browse` command wraps it).
PLAYWRIGHT_VERSION := 1.64.1
# pi (agent.kind: pi), from npm; it doesn't update itself.
PI_VERSION := 1.3.0
# Node.js LTS from nodejs.org (Ubuntu's is too old for Playwright).
NODE_VERSION := v22.23.3
NODE_SHA256 := df450af89261115ef9f9e3830c3eeb2cc9213b63c720b1af623cb5dcbe2e02de
KUBECTL_VERSION := v1.36.5
KUBECTL_SHA256 := 33bc88a24c3b09cf55bfd59e6ca977881b61fc3f25ab65bc80c65b430248648e
IMAGE_FROM_office := docker.io/library/ubuntu:26.04
IMAGE_RUN_office  := --run "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl git less procps ripgrep jq make xz-utils python3 python3-pip python3-venv podman buildah fuse-overlayfs uidmap && rm -rf /var/lib/apt/lists/*" \
	--run "curl -fsSL -o /tmp/node.tar.xz https://nodejs.org/dist/$(NODE_VERSION)/node-$(NODE_VERSION)-linux-x64.tar.xz && echo '$(NODE_SHA256)  /tmp/node.tar.xz' | sha256sum -c && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 --exclude='*.md' --exclude=LICENSE && rm /tmp/node.tar.xz && node --version" \
	--run "curl -fsSL -o /usr/local/bin/kubectl https://dl.k8s.io/release/$(KUBECTL_VERSION)/bin/linux/amd64/kubectl && echo '$(KUBECTL_SHA256)  /usr/local/bin/kubectl' | sha256sum -c && chmod 755 /usr/local/bin/kubectl" \
	--run "curl -fsSL https://claude.ai/install.sh | bash -s $(CLAUDE_VERSION) && cp -L /root/.local/bin/claude /usr/local/bin/claude && rm -rf /root/.local /root/.claude* && claude --version" \
	--run "npm install -g --ignore-scripts --no-audit --no-fund @earendil-works/pi-coding-agent@$(PI_VERSION) && PI_OFFLINE=1 pi --version && rm -rf /root/.npm /root/.pi" \
	--run "npm install -g --no-audit --no-fund playwright@$(PLAYWRIGHT_VERSION) && PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright playwright install --with-deps chromium && chmod -R a+rX /opt/ms-playwright && rm -rf /var/lib/apt/lists/* /root/.npm"

# The cluster facility: k3s at a pinned release, with its air-gap images so it
# starts without the internet. Needs the k8s kernel: make kernel KERNEL=k8s.
K3S_VERSION := v1.36.5%2Bk3s1
K3S_SHA256 := d73847bcd3c5fccef0115b372e2f9a91f3032dc84bbf71518a4617565294d313
K3S_IMAGES_SHA256 := 7d939471907df2d4e537a9c308bf94173e453fff4a2e9e77e9ae92a64be3007f
K3S_URL := https://github.com/k3s-io/k3s/releases/download/$(K3S_VERSION)
# The board's dashboard: Headlamp, whose image goes into the air-gap
# directory so k3s imports it at start.
HEADLAMP_IMAGE := ghcr.io/headlamp-k8s/headlamp:v0.46.0
HEADLAMP_DIGEST := sha256:8a0ef97527544baa2b35825940ddf4db6e2d09c30cd5aa31fd269ff604cc8b75
IMAGE_FROM_k3s := docker.io/library/ubuntu:26.04
IMAGE_RUN_k3s  := --run "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl iptables conntrack ipset zstd procps && rm -rf /var/lib/apt/lists/*" \
	--run "curl -fsSL -o /usr/local/bin/k3s $(K3S_URL)/k3s && echo '$(K3S_SHA256)  /usr/local/bin/k3s' | sha256sum -c && chmod 755 /usr/local/bin/k3s && ln -sf k3s /usr/local/bin/kubectl" \
	--run "mkdir -p /var/lib/rancher/k3s/agent/images && curl -fsSL -o /var/lib/rancher/k3s/agent/images/k3s-airgap-images-amd64.tar.zst $(K3S_URL)/k3s-airgap-images-amd64.tar.zst && echo '$(K3S_IMAGES_SHA256)  /var/lib/rancher/k3s/agent/images/k3s-airgap-images-amd64.tar.zst' | sha256sum -c" \
	--run "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends skopeo && skopeo copy --override-os linux --override-arch amd64 docker://$(firstword $(subst :, ,$(HEADLAMP_IMAGE)))@$(HEADLAMP_DIGEST) docker-archive:/var/lib/rancher/k3s/agent/images/headlamp.tar:$(HEADLAMP_IMAGE) && apt-get purge -y skopeo && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*"

.PHONY: deps build install image kernel check test lint fmt hooks test-e2e test-e2e-office clean

deps:
	scripts/fetch-deps.sh

build:
	cargo build --release $(FEATURES) -p zapper-agent --target $(MUSL)
	cargo build --release $(FEATURES) $(HOST)
	cp $(AGENT) $(TARGET)/zapper-agent

install: build
	install -d $(PREFIX)/bin
	install -m 0755 $(addprefix $(TARGET)/,$(BINS)) $(PREFIX)/bin/

image: build
	@test -n "$(IMAGE_FROM_$(IMAGE))" || { echo "no recipe for IMAGE=$(IMAGE); see IMAGE_FROM_* in the Makefile, or run zapper image build"; exit 1; }
	$(ZAPPER) image build $(IMAGE) --from $(IMAGE_FROM_$(IMAGE)) $(IMAGE_RUN_$(IMAGE))

kernel:
	scripts/build-kernel.sh $(KERNEL)

check: build
	$(ZAPPER) check

test:
	cargo test --workspace $(EXCLUDE) $(FEATURES)

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace $(EXCLUDE) --all-targets --release $(FEATURES) -- -D warnings

fmt:
	cargo fmt --all

hooks:
	git config core.hooksPath .githooks

test-e2e: build
	tests/e2e.sh $(ZAPPER)
	tests/e2e-office.sh $(ZAPPER)

test-e2e-office: build
	tests/e2e-office.sh $(ZAPPER)

clean:
	cargo clean
