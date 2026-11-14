# Build and run cluster-backend + simulation-frontend (with Web UI).
# Ctrl+C stops both (and their process groups).

.PHONY: all build build-rust build-web run help

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
CARGO ?= cargo
NPM ?= npm

BACKEND_BIN := cluster-backend
FRONTEND_BIN := simulation-frontend
BACKEND_CONFIG ?= $(ROOT)/example/config/backend.yaml
FRONTEND_CONFIG ?= $(ROOT)/example/config/frontend.yaml

ifeq ($(RELEASE),1)
CARGO_FLAGS := --release
TARGET_DIR := $(ROOT)/target/release
else
CARGO_FLAGS :=
TARGET_DIR := $(ROOT)/target/debug
endif

all: run

help:
	@echo "Targets:"
	@echo "  make build       Build Rust bins and Web UI"
	@echo "  make build-rust  Build cluster-backend and simulation-frontend"
	@echo "  make build-web   npm ci + build sim-cluster/frontend -> sim-cluster/out/web"
	@echo "  make run         Build then start cluster-backend and simulation-frontend"
	@echo "  make run RELEASE=1   Same with release binaries"
	@echo ""
	@echo "Configs (override with BACKEND_CONFIG=... FRONTEND_CONFIG=...):"
	@echo "  $(BACKEND_CONFIG)"
	@echo "  $(FRONTEND_CONFIG)"
	@echo ""
	@echo "Ctrl+C on 'make run' terminates both processes."

build: build-rust build-web

build-rust:
	git submodule update --init --resursive
	cd "$(ROOT)" && $(CARGO) build --workspace $(CARGO_FLAGS)

build-web:
	cd "$(ROOT)/sim-cluster/frontend" && $(NPM) ci && $(NPM) run build

run: build
	@bash -eu -o pipefail -c '\
	  pids=""; \
	  cleanup() { \
	    status=$$?; \
	    trap - INT TERM EXIT; \
	    if [ -n "$$pids" ]; then \
	      echo; \
	      echo "Stopping cluster-backend and simulation-frontend..."; \
	      for pid in $$pids; do \
	        kill -TERM -- -$$pid 2>/dev/null || kill -TERM $$pid 2>/dev/null || true; \
	      done; \
	      for pid in $$pids; do \
	        wait $$pid 2>/dev/null || true; \
	      done; \
	    fi; \
	    exit $$status; \
	  }; \
	  trap cleanup INT TERM EXIT; \
	  setsid "$(TARGET_DIR)/$(BACKEND_BIN)" --config "$(BACKEND_CONFIG)" & \
	  pids="$$!"; \
	  echo "Started $(BACKEND_BIN) (pid $$pids, config $(BACKEND_CONFIG))"; \
	  sleep 5; \
	  setsid "$(TARGET_DIR)/$(FRONTEND_BIN)" --config "$(FRONTEND_CONFIG)" & \
	  fp="$$!"; \
	  pids="$$pids $$fp"; \
	  echo "Started $(FRONTEND_BIN) (pid $$fp, config $(FRONTEND_CONFIG))"; \
	  wait $$pids; \
	  trap - INT TERM EXIT; \
	'
