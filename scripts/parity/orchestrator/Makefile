ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/../../..)

.PHONY: build-debug
build-debug:
	cd $(ROOT) && cargo build --release --locked -p orchestrator
	mkdir -p bin
	cp $(ROOT)/target/release/orchestrator bin/orchestrator

.PHONY: run-debug
run-debug:
	make build-debug
	sudo -E NODE_ID="testclient" ./bin/orchestrator
