# DaBoss Browser workflow. All headless work goes through Docker; the windowed
# binary runs on the macOS host under sandbox-exec from phase 5 onwards.

default:
    @just --list

# Build (or rebuild) the dev image. Run after Dockerfile changes.
image:
    docker compose build

# Build the project inside Docker.
build:
    docker compose run --rm dev cargo build

build-release:
    docker compose run --rm dev cargo build --release

# Run the test suite inside Docker.
test:
    docker compose run --rm dev cargo test

# Supply-chain checks. Run before every commit.
audit:
    docker compose run --rm dev cargo deny check
    docker compose run --rm dev cargo audit

# Drop into a shell inside the dev container.
shell:
    docker compose run --rm dev bash

# Phase 0 only: build and run the windowed binary on the host so we can sanity-
# check that winit+softbuffer actually open a window on this machine. From
# phase 5 onwards use `run-sandboxed` instead.
run-host:
    cargo run

# Phase 5+: launch the release binary on the macOS host under Seatbelt.
# Requires `cargo build --release` on the host (not the Docker Linux build).
run-sandboxed:
    sandbox-exec -f profiles/macos.sb ./target/release/daboss
