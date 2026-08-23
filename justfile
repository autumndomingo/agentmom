set dotenv-load := false

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

smoke-skills:
    nix develop -c npm run smoke:skills

smoke-technically-speaking:
    nix develop -c npm run smoke:technically-speaking

smoke-local:
    nix develop -c npm run smoke:local

smoke-deploy:
    nix develop -c npm run smoke:deploy

start:
    nix develop -c npm run start

deploy-prod:
    fly deploy --remote-only --yes
    just check-prod

check-prod:
    #!/usr/bin/env bash
    set -euo pipefail
    health="$(curl -fsS --max-time 20 https://agentmom.xyz/api/health)"
    echo "$health"
    [[ "$health" == *'"ok":true'* ]]
    me_status="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://agentmom.xyz/api/me)"
    [[ "$me_status" == "401" ]]
    tutorial_status="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://agentmom.xyz/technically-speaking/)"
    [[ "$tutorial_status" == "302" ]]
    tutorial_api_status="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://agentmom.xyz/technically-speaking/api/config)"
    [[ "$tutorial_api_status" == "401" ]]
    echo "prod check ok"

logs-prod:
    fly logs -a agentmom-web
