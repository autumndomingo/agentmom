set dotenv-load

default:
    @just --list

ui-build:
    npm --prefix ui run build

dev:
    @if ! command -v lsof >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then exec nix develop --command just dev; fi
    @./scripts/dev-run

dev-smoke:
    @./scripts/dev-smoke

real-fleet-test:
    cargo test --test real_fleet real_api_health_metrics_and_worker_auth -- --ignored --test-threads=1
    cargo test --test real_fleet real_unknown_explicit_node_is_rejected -- --ignored --test-threads=1

real-fleet-test-mutating:
    cargo test --test real_fleet -- --ignored --test-threads=1
