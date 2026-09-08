VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DOCKER  ?= docker
BOX     ?= placard-01
BUILD_IMAGE := placard-build
# The target box and CI are amd64; on Apple Silicon this runs via
# Rosetta/qemu so goldens and deploy binaries match what CI produces.
PLATFORM := linux/amd64
IN_CONTAINER = $(DOCKER) run --rm --platform $(PLATFORM) \
	-v $(PWD):/src -v placard-cargo:/root/.cargo/registry -w /src $(BUILD_IMAGE)

.PHONY: run test goldens linux-bin deploy-dev release build-image

run:
	cargo run -- --config packaging/config.toml.example --sink auto --state-dir ./state

test:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

build-image:
	$(DOCKER) build --platform $(PLATFORM) -t $(BUILD_IMAGE) -f Dockerfile.build .

goldens: build-image
	$(IN_CONTAINER) sh -c 'cargo build --release --target-dir target/linux-docker && \
		for f in tests/scenes/*.json; do \
		    target/linux-docker/release/placard --config packaging/config.toml.example \
		        --snapshot "$$f" "tests/golden/$$(basename "$$f" .json).png"; \
		done'

linux-bin: build-image
	$(IN_CONTAINER) sh -c 'cargo build --release --target-dir target/linux-docker'
	mkdir -p target/linux
	cp target/linux-docker/release/placard target/linux/placard

deploy-dev: linux-bin
	scp target/linux/placard root@$(BOX):/usr/bin/placard.new
	ssh root@$(BOX) 'mv /usr/bin/placard.new /usr/bin/placard && systemctl restart placard && journalctl -u placard -f'

# Every push to main releases; this is just the explicit spelling of that.
release:
	git push origin main
