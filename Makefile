# Makefile for iterating the neolink fork on the home k3s lab.
#
# Pattern (one per app repo, per k3s/README.md "Building and pushing custom images"):
#   make image          # build the native (FROM scratch) image locally
#   make push           # build + push registry.home.devoops.co/neolink:<sha>
#   make deploy         # push, then update helmrelease.yml + git commit + push
#   make version        # print the short git SHA used as the image tag
#
# Requires: podman, git, a rust toolchain (the native image builds on
# rust:1.91.0-alpine3.22 inside podman, so no local Rust is needed).

REGISTRY   ?= registry.home.devoops.co
IMAGE      ?= $(REGISTRY)/neolink
DOCKERFILE ?= Dockerfile.native
PLATFORM   ?= linux/amd64
SHA        := $(shell git rev-parse --short HEAD 2>/dev/null || echo dirty)
TAG        ?= $(SHA)

.PHONY: help image push deploy version clean check test

help:
	@echo "Targets:"
	@echo "  image    - build the native image locally (tagged $(IMAGE):$(TAG))"
	@echo "  push     - build and push to the home registry"
	@echo "  deploy   - push, bump helmrelease.yml image.tag, commit and push"
	@echo "  version  - print the image tag (short git SHA)"
	@echo "  check    - cargo check for both backends (needs local Rust)"
	@echo "  test     - cargo test for both backends (needs local Rust + GStreamer deps)"
	@echo "  clean    - remove the local image"

version:
	@echo $(TAG)

image:
	podman build --platform $(PLATFORM) \
		--tag $(IMAGE):$(TAG) \
		--file $(DOCKERFILE) .
	podman tag $(IMAGE):$(TAG) $(IMAGE):latest

push: image
	podman push $(IMAGE):$(TAG)
	podman push $(IMAGE):latest

deploy: push
	@if [ -z "$(TAG)" ] || [ "$(TAG)" = "dirty" ]; then \
		echo "Refusing to deploy from a dirty tree: commit first."; exit 1; \
	fi
	@cd ../k3s && \
		sed -i 's/^\s*tag:.*/      tag: "$(TAG)"/' cluster/neolink/helmrelease.yml && \
		git add cluster/neolink/helmrelease.yml && \
		git commit -m "chore(neolink): deploy image $(TAG)" && \
		git push
	@echo "Deployed $(IMAGE):$(TAG). Flux reconciles cluster/neolink within ~1 minute."

check:
	cargo check --locked --workspace
	cargo check --locked --workspace --no-default-features --features native-rtsp

test:
	cargo test --locked --workspace
	cargo test --locked --workspace --features native-rtsp,pushnoti

clean:
	podman rmi $(IMAGE):$(TAG) $(IMAGE):latest 2>/dev/null || true
