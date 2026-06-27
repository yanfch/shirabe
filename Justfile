set shell := ["bash", "-cu"]

bind := "127.0.0.1:7778"
ui_port := "5173"

default:
    @just --list

fmt:
    cargo fmt

test:
    cargo test

build:
    cargo build

ui-install:
    cd ui && npm install

ui-build:
    cd ui && npm run build

check: fmt test ui-build
    git diff --check

init:
    cargo run -- init

pricing:
    cargo run -- pricing refresh

rollup:
    cargo run -- rollup refresh

import source path="":
    if [ -n "{{path}}" ]; then \
      cargo run -- import "{{source}}" --path "{{path}}"; \
    else \
      cargo run -- import "{{source}}"; \
    fi

import-codex:
    cargo run -- import codex

import-pi:
    cargo run -- import pi

import-claude:
    cargo run -- import claude

import-kanade path="":
    if [ -n "{{path}}" ]; then \
      cargo run -- import kanade --path "{{path}}"; \
    else \
      cargo run -- import kanade; \
    fi

import-all: import-codex import-pi import-claude import-kanade
    cargo run -- rollup refresh

stop:
    pkill -x shirabe || true
    pkill -x ShirabeServer || true

start bind=bind: build ui-build
    pkill -x ShirabeServer || true
    log_path="${SHIRABE_DIR:-$HOME/.shirabe}/shirabe.log"; mkdir -p "$(dirname "$log_path")"; python3 -c 'import os, subprocess, sys; log = open(sys.argv[1], "ab"); subprocess.Popen(["./target/debug/shirabe", "serve", "--bind", "{{bind}}", "--ui-dir", "ui/dist"], cwd=os.getcwd(), stdout=log, stderr=subprocess.STDOUT, start_new_session=True)' "$log_path"
    log_path="${SHIRABE_DIR:-$HOME/.shirabe}/shirabe.log"; for _ in {1..50}; do curl -fs "http://{{bind}}/api/health" >/dev/null && { echo "shirabe is running at http://{{bind}}"; exit 0; }; sleep 0.2; done; tail -40 "$log_path"; exit 1

server bind=bind:
    cargo run -- serve --bind "{{bind}}" --ui-dir ui/dist

serve bind=bind: ui-build
    cargo run -- serve --bind "{{bind}}" --ui-dir ui/dist

restart bind=bind: build ui-build
    pkill -x shirabe || true
    pkill -x ShirabeServer || true
    sleep 0.2
    log_path="${SHIRABE_DIR:-$HOME/.shirabe}/shirabe.log"; mkdir -p "$(dirname "$log_path")"; python3 -c 'import os, subprocess, sys; log = open(sys.argv[1], "ab"); subprocess.Popen(["./target/debug/shirabe", "serve", "--bind", "{{bind}}", "--ui-dir", "ui/dist"], cwd=os.getcwd(), stdout=log, stderr=subprocess.STDOUT, start_new_session=True)' "$log_path"
    log_path="${SHIRABE_DIR:-$HOME/.shirabe}/shirabe.log"; for _ in {1..50}; do curl -fs "http://{{bind}}/api/health" >/dev/null && { echo "shirabe is running at http://{{bind}}"; exit 0; }; sleep 0.2; done; tail -40 "$log_path"; exit 1

ui-dev port=ui_port:
    cd ui && npm run dev -- --port "{{port}}"

macos-build:
    cd app/macos/ShirabeBar && swift build

macos-run:
    ./script/build_and_run.sh

macos-verify:
    ./script/build_and_run.sh --verify

package-cli:
    ./script/package_cli.sh

package-macos:
    ./script/package_macos_dmg.sh

package:
    rm -rf dist/release
    just package-cli
    just package-macos
    cd dist/release && shasum -a 256 *.tar.gz *.dmg > SHA256SUMS
