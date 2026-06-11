set dotenv-load

port := env_var_or_default("MOM_UI_PORT", "8787")

default:
    @just --list

ui-build:
    npm --prefix ui run build

dev: ui-build
    @echo "Starting Agent Mom UI on http://127.0.0.1:{{port}}"
    cargo run --bin mom-ui
