set dotenv-load

default:
    @just --list

ui-build:
    npm --prefix ui run build

pre-merge:
    @if ! command -v cargo >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then exec nix develop --command just pre-merge; fi
    cargo fmt --check
    cargo check
    cargo test
    npm --prefix ui run build
    bash -n scripts/measure-suspended-message-e2e

dev:
    @if ! command -v lsof >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1 || ! command -v iron-proxy >/dev/null 2>&1 || ! command -v playwright >/dev/null 2>&1; then exec nix develop --command just dev; fi
    @./scripts/dev-run

dev-utm:
    @if ! command -v lsof >/dev/null 2>&1 || ! command -v rsync >/dev/null 2>&1 || ! command -v ssh >/dev/null 2>&1 || ! command -v utmctl >/dev/null 2>&1; then exec nix develop --command just dev-utm; fi
    @./scripts/dev-utm

dev-utm-list:
    @if ! command -v utmctl >/dev/null 2>&1; then exec nix develop --command just dev-utm-list; fi
    @utmctl list | awk 'NR == 1 || /AgentMom-/ { print }'

dev-utm-e2e:
    @if ! command -v cargo >/dev/null 2>&1; then exec nix develop --command just dev-utm-e2e; fi
    @AGENTMOM_UTM_E2E=1 AGENTMOM_UTM_API_URL="${AGENTMOM_UTM_API_URL:-http://127.0.0.1:${MOM_API_PORT:-8787}}" cargo test --test utm_e2e -- --ignored --test-threads=1

dev-reset:
    @./scripts/dev-reset

dev-smoke:
    @./scripts/dev-smoke

stage-host-dev:
    @./scripts/stage-host-dev-run

stage-host-dev-reset:
    @./scripts/stage-host-dev-reset

real-fleet-test:
    cargo test --test real_fleet real_api_health_metrics_and_worker_auth -- --ignored --test-threads=1

real-fleet-test-mutating:
    cargo test --test real_fleet -- --ignored --test-threads=1

fleet-build selector="@agentmom":
    nix develop --command colmena build --on {{selector}}

fleet-build-stage:
    just fleet-build @stage

fleet-build-prod:
    just fleet-build @prod

deploy selector:
    nix develop --command colmena apply --on {{selector}}

deploy-stage:
    just deploy @stage

deploy-prod:
    just deploy @prod

deploy-workers:
    just deploy @worker

deploy-ctrl:
    just deploy @ctrl

deploy-node node:
    just deploy {{node}}

fleet-status selector="@agentmom":
    nix develop --command colmena exec --on {{selector}} -- systemctl --no-pager --failed

real-fleet-test-prod:
    AGENTMOM_REAL_NODE_A=mom-1 AGENTMOM_REAL_WORKER_TOKEN="$(ssh mom-ctrl 'sudo cat /run/agenix/agentmom-worker-token-mom-1')" just real-fleet-test
    AGENTMOM_REAL_NODE_A=mom-2 AGENTMOM_REAL_WORKER_TOKEN="$(ssh mom-ctrl 'sudo cat /run/agenix/agentmom-worker-token-mom-2')" just real-fleet-test

real-fleet-test-prod-mutating:
    AGENTMOM_REAL_NODE_A=mom-1 AGENTMOM_REAL_WORKER_TOKEN="$(ssh mom-ctrl 'sudo cat /run/agenix/agentmom-worker-token-mom-1')" just real-fleet-test-mutating
    AGENTMOM_REAL_NODE_A=mom-2 AGENTMOM_REAL_WORKER_TOKEN="$(ssh mom-ctrl 'sudo cat /run/agenix/agentmom-worker-token-mom-2')" just real-fleet-test-mutating

stage-e2e-suspend-latency workspace="stage-e2e" vm="mom-stage-e2e":
    AGENTMOM_E2E_API_URL=https://stage.agentmom.xyz \
    AGENTMOM_E2E_CTRL_SSH=justin@204.168.131.33 \
    AGENTMOM_E2E_WORKER_SSH=justin@100.92.189.28 \
    AGENTMOM_E2E_KEEP_WS_OPEN=1 \
    AGENTMOM_E2E_GUEST_PING=1 \
    ./scripts/measure-suspended-message-e2e {{workspace}} {{vm}}
