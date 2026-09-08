VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DOCKER  ?= docker
BOX     ?= placard-01

# Only the deploy artefact must be amd64 (the box is an Intel N150); goldens
# and local container runs use the host's native architecture — rendering is
# deterministic across arches within the golden tolerance, and CI re-asserts
# every push on amd64. Platforms are always explicit because a cached base
# image otherwise decides silently.
NATIVE_PLATFORM := linux/$(shell uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')
DEPLOY_PLATFORM := linux/amd64
BUILD_IMAGE  := placard-build
DEPLOY_IMAGE := placard-build-amd64

NATIVE_RUN = $(DOCKER) run --rm --platform $(NATIVE_PLATFORM) \
	-v $(PWD):/src -v placard-cargo:/root/.cargo/registry -w /src $(BUILD_IMAGE)
DEPLOY_RUN = $(DOCKER) run --rm --platform $(DEPLOY_PLATFORM) \
	-v $(PWD):/src -v placard-cargo:/root/.cargo/registry -w /src $(DEPLOY_IMAGE)

.PHONY: run test goldens linux-bin deploy-dev release build-image build-image-amd64

run:
	cargo run -- --config packaging/config.toml.example --sink auto --state-dir ./state

test:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

build-image:
	$(DOCKER) build --platform $(NATIVE_PLATFORM) -t $(BUILD_IMAGE) -f Dockerfile.build .

build-image-amd64:
	$(DOCKER) build --platform $(DEPLOY_PLATFORM) -t $(DEPLOY_IMAGE) -f Dockerfile.build .

goldens: build-image
	$(NATIVE_RUN) sh -c 'cargo build --release --target-dir target/linux-native && \
		for f in tests/scenes/*.json; do \
		    target/linux-native/release/placard --config packaging/config.toml.example \
		        --snapshot "$$f" "tests/golden/$$(basename "$$f" .json).png"; \
		done'

linux-bin: build-image-amd64
	$(DEPLOY_RUN) sh -c 'cargo build --release --target-dir target/linux-amd64'
	mkdir -p target/linux
	cp target/linux-amd64/release/placard target/linux/placard

deploy-dev: linux-bin
	scp target/linux/placard root@$(BOX):/usr/bin/placard.new
	ssh root@$(BOX) 'mv /usr/bin/placard.new /usr/bin/placard && systemctl restart placard && journalctl -u placard -f'

# Every push to main releases; this is just the explicit spelling of that.
release:
	git push origin main
