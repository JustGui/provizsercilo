REGISTRY  := justgu1
VERSION   := v0.4.0

SERVER_IMAGE  := $(REGISTRY)/proviz-sercilo
BRIDGE_IMAGE  := $(REGISTRY)/ddg-bridge
SEARXNG_IMAGE := $(REGISTRY)/searxng

.PHONY: build build-server build-bridge build-searxng \
        push push-server push-bridge push-searxng \
        release release-multiarch release-amd64

# ── Build ────────────────────────────────────────────────────────────────────

build: build-server build-bridge build-searxng

build-server:
	docker build \
		-t $(SERVER_IMAGE):latest \
		-t $(SERVER_IMAGE):$(VERSION) \
		.

build-bridge:
	docker build \
		-t $(BRIDGE_IMAGE):latest \
		-t $(BRIDGE_IMAGE):$(VERSION) \
		bridges/ddgs-bridge

build-searxng:
	docker build \
		-t $(SEARXNG_IMAGE):latest \
		-t $(SEARXNG_IMAGE):$(VERSION) \
		searxng

# ── Push ─────────────────────────────────────────────────────────────────────

push: push-server push-bridge push-searxng

push-server:
	docker push $(SERVER_IMAGE):$(VERSION)
	docker push $(SERVER_IMAGE):latest

push-bridge:
	docker push $(BRIDGE_IMAGE):$(VERSION)
	docker push $(BRIDGE_IMAGE):latest

push-searxng:
	docker push $(SEARXNG_IMAGE):$(VERSION)
	docker push $(SEARXNG_IMAGE):latest

# ── Build + push ─────────────────────────────────────────────────────────────

release: build push

# Multi-arch (linux/amd64 + linux/arm64) — requires `docker buildx`
release-multiarch:
	docker buildx build \
		--platform linux/amd64,linux/arm64 \
		--push \
		-t $(SERVER_IMAGE):latest \
		-t $(SERVER_IMAGE):$(VERSION) \
		.
	docker buildx build \
		--platform linux/amd64,linux/arm64 \
		--push \
		-t $(BRIDGE_IMAGE):latest \
		-t $(BRIDGE_IMAGE):$(VERSION) \
		bridges/ddgs-bridge
	docker buildx build \
		--platform linux/amd64,linux/arm64 \
		--push \
		-t $(SEARXNG_IMAGE):latest \
		-t $(SEARXNG_IMAGE):$(VERSION) \
		searxng

# linux/amd64 only — explicit amd64 tags alongside multiarch
release-amd64:
	docker buildx build \
		--platform linux/amd64 \
		--push \
		-t $(SERVER_IMAGE):latest-amd64 \
		-t $(SERVER_IMAGE):$(VERSION)-amd64 \
		.
	docker buildx build \
		--platform linux/amd64 \
		--push \
		-t $(BRIDGE_IMAGE):latest-amd64 \
		-t $(BRIDGE_IMAGE):$(VERSION)-amd64 \
		bridges/ddgs-bridge
	docker buildx build \
		--platform linux/amd64 \
		--push \
		-t $(SEARXNG_IMAGE):latest-amd64 \
		-t $(SEARXNG_IMAGE):$(VERSION)-amd64 \
		searxng
