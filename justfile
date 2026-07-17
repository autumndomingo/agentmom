set dotenv-load := false

prod_host := "justin@mom-1.quokka-fir.ts.net"
stage_host := "justin@100.92.189.28"

dev:
    nix develop -c npm run dev

dev-auth:
    AGENTMOM_AUTH_ENABLED=1 AGENTMOM_STATE_DIR=.agentmom-auth AGENTMOM_DEV_AUTH_PASSWORD=password AGENTMOM_DEV_AUTH_USERS='admin@bitcoin.com|Admin User|admin,user@bitcoin.com|Normal User|user' nix develop -c npm run dev

install:
    nix develop -c npm install

build:
    nix develop -c npm run build

typecheck:
    nix develop -c npm run typecheck

smoke-auth:
    nix develop -c npm run smoke:auth

smoke-cli:
    nix develop -c npm run smoke:cli

smoke-web-search:
    nix develop -c npm run smoke:web-search

smoke-session-switch:
    nix develop -c npm run smoke:session-switch

smoke-local:
    nix develop -c npm run smoke:local

smoke-smolvm:
    nix develop -c npm run smoke:smolvm

smoke-deploy:
    nix develop -c npm run smoke:deploy

podman:
    nix develop -c scripts/ensure-podman-machine.sh

start:
    nix develop -c npm run start

fleet-build-prod:
    nix develop -c colmena build --on mom-1

fleet-build-stage:
    nix develop -c colmena build --on mom-stage-1

fleet-status-prod:
    nix develop -c colmena exec --on mom-1 -- systemctl --no-pager --failed

fleet-status-stage:
    nix develop -c colmena exec --on mom-stage-1 -- systemctl --no-pager --failed

check-prod:
    #!/usr/bin/env bash
    set -euo pipefail

    ssh {{prod_host}} 'bash -se' <<'REMOTE'
    set -euo pipefail

    for attempt in $(seq 1 60); do
      health_json="$(curl -fsS http://127.0.0.1:7392/api/health || true)"
      if [[ "${health_json}" == *'"ok":true'* ]]; then
        echo "${health_json}"
        break
      fi
      if [[ "${attempt}" == "60" ]]; then
        echo "agentmom health check failed" >&2
        [[ -n "${health_json}" ]] && echo "${health_json}" >&2
        systemctl status agentmom --no-pager -n 80 >&2 || true
        exit 1
      fi
      sleep 1
    done

    me_status="$(curl -sS -o /tmp/agentmom-deploy-me.json -w '%{http_code}' http://127.0.0.1:7392/api/me)"
    if [[ "${me_status}" != "401" ]]; then
      echo "expected unauthenticated /api/me to return 401, got ${me_status}" >&2
      cat /tmp/agentmom-deploy-me.json >&2
      exit 1
    fi
    grep -q '"authEnabled":true' /tmp/agentmom-deploy-me.json

    echo "prod check ok"
    REMOTE

check-stage:
    #!/usr/bin/env bash
    set -euo pipefail

    ssh {{stage_host}} 'bash -se' <<'REMOTE'
    set -euo pipefail

    for attempt in $(seq 1 60); do
      health_json="$(curl -fsS http://127.0.0.1:7392/api/health || true)"
      if [[ "${health_json}" == *'"ok":true'* ]]; then
        echo "${health_json}"
        break
      fi
      if [[ "${attempt}" == "60" ]]; then
        echo "agentmom health check failed" >&2
        [[ -n "${health_json}" ]] && echo "${health_json}" >&2
        sudo systemctl status agentmom --no-pager -n 80 >&2 || true
        exit 1
      fi
      sleep 1
    done

    me_status="$(curl -sS -o /tmp/agentmom-deploy-me.json -w '%{http_code}' http://127.0.0.1:7392/api/me)"
    if [[ "${me_status}" != "401" ]]; then
      echo "expected unauthenticated /api/me to return 401, got ${me_status}" >&2
      cat /tmp/agentmom-deploy-me.json >&2
      exit 1
    fi
    grep -q '"authEnabled":true' /tmp/agentmom-deploy-me.json

    echo "stage deploy ok"
    REMOTE

deploy-stage:
    nix develop -c colmena apply --on mom-stage-1
    nix develop -c colmena exec --on mom-stage-1 -- sudo systemctl restart agentmom
    just check-stage

deploy-prod:
    nix develop -c colmena apply --on mom-1
    nix develop -c colmena exec --on mom-1 -- sudo systemctl restart agentmom
    just check-prod
