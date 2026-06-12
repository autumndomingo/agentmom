set dotenv-load

port := env_var_or_default("MOM_UI_PORT", "8787")

default:
    @just --list

ui-build:
    npm --prefix ui run build

dev: ui-build
    @echo "Starting Agent Mom UI on http://127.0.0.1:{{port}}"
    MOM_UI_PORT={{port}} cargo run --bin mom-ui

real-fleet-test:
    cargo test --test real_fleet real_api_health_metrics_and_worker_auth -- --ignored --test-threads=1
    cargo test --test real_fleet real_unknown_explicit_node_is_rejected -- --ignored --test-threads=1

real-fleet-test-mutating:
    cargo test --test real_fleet -- --ignored --test-threads=1
