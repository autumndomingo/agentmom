set dotenv-load

default:
    @just --list

ui-build:
    npm --prefix ui ci
    npm --prefix ui run build

fmt-check:
    cargo fmt --check

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test --all-targets

nix-check:
    nix flake check --no-build --all-systems

pre-merge: fmt-check clippy test nix-check
    git diff --check

dev:
    @if ! command -v lsof >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1 || ! command -v iron-proxy >/dev/null 2>&1 || ! command -v playwright >/dev/null 2>&1; then exec nix develop --command just dev; fi
    @./scripts/dev-run

dev-reset:
    @./scripts/dev-reset

dev-smoke:
    @./scripts/dev-smoke

real-fleet-test:
    cargo test --test real_fleet real_api_health_metrics_and_worker_auth -- --ignored --test-threads=1
    cargo test --test real_fleet real_unknown_explicit_node_is_rejected -- --ignored --test-threads=1

real-fleet-test-mutating:
    cargo test --test real_fleet -- --ignored --test-threads=1
