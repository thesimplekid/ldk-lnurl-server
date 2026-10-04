set positional-arguments

default:
    @just --list

build *args="":
    cargo build --all-targets {{args}}

check:
    cargo check --all-targets

format:
    cargo fmt --all
    nix fmt

lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings

test:
    cargo test --all-targets
    cargo test --doc

quick-check: lint test

final-check: quick-check
    nix flake check path:. -L

run:
    cargo run
