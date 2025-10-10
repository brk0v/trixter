DIST_DIR = dist
COMMIT := $(shell git rev-parse HEAD)
DOCKER_IMAGE= ghcr.io/brk0v/trixter
VERSION := $(shell cat trixter/Cargo.toml | grep '^version =' | cut -d'"' -f2)

.PHONY: fmt
fmt:
	cargo fmt --all --check

.PHONY: check
check:
	cargo check

.PHONY: clippy
clippy:
	cargo clippy --all-targets

.PHONY: clean
clean:
	rm -rf $(DIST_DIR)

.PHONY: test
test:
	cargo test --workspace

.PHONY: update_deps
update_deps:
	cargo update

.PHONY: release
release:
	date
	git checkout main
	git push
	git checkout release
	git merge main
	git push
	git tag $(VERSION)
	git push --tags
	git checkout main


# Build

.PHONY: build
build:
	mkdir -p $(DIST_DIR)
	cargo build -p trixter --release
	cp target/release/trixter $(DIST_DIR)/
	strip --strip-all -xX $(DIST_DIR)/trixter


.PHONY: docker_build
docker_build:
	docker build -t $(DOCKER_IMAGE):latest -f Dockerfile .
	docker tag $(DOCKER_IMAGE):latest $(DOCKER_IMAGE):$(VERSION)


.PHONY: docker_push
docker_push:
	docker push $(DOCKER_IMAGE):latest $(DOCKER_IMAGE):$(VERSION)
