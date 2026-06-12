set dotenv-load

port := env_var_or_default("MOM_UI_PORT", "8787")

default:
    @just --list

ui-build:
    npm --prefix ui run build

dev: ui-build
    @echo "Starting Agent Mom API/UI on http://127.0.0.1:{{port}}"
    MOM_UI_DIST=ui/dist cargo run --bin mom -- api --bind 127.0.0.1:{{port}}

real-fleet-test:
    cargo test --test real_fleet -- --ignored --nocapture
