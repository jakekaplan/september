set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# List available recipes without running checks.
default:
    @just --list

# Format Rust sources.
fmt:
    cargo fmt --all

# Check formatting without modifying sources.
fmt-check:
    cargo fmt --all -- --check

# Lint one affected package, including its test targets.
lint package:
    cargo clippy --locked --package {{quote(package)}} --all-targets --all-features -- -D warnings

# Run an affected package's tests, optionally matching a test-name filter.
test package filter="":
    cargo test --locked --package {{quote(package)}} {{quote(filter)}}

# Check file sizes across the repository, excluding generated lockfiles.
files:
    uv tool run --from 'loq>=0.1.0' loq check .

# Local checks for one affected package; does not run its tests.
check package: fmt-check (lint package) files

# Check dependency advisories, licenses, versions, and sources.
deny:
    cargo deny --locked check

# Install optional commit hooks for this checkout.
hooks-install:
    uv tool run --from 'prek>=0.5.5' prek install

# Run hooks against explicitly named changed files.
[positional-arguments]
hooks +paths:
    uv tool run --from 'prek>=0.5.5' prek run --files "$@"

# Explicitly opt into CI-equivalent validation; not the default local workflow.
ci: fmt-check files
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    cargo test --locked --workspace
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
    cargo deny --locked check
