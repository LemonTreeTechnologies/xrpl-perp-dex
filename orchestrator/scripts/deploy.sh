#!/usr/bin/env bash
set -euo pipefail

# Deploy perp-dex-orchestrator to Azure testnet cluster.
#
# Usage:
#   ./scripts/deploy.sh [node-1|node-2|node-3|all]
#
# Prerequisites:
#   - SSH access via Hetzner bastion (94.130.18.162)
#   - Binary built on Hetzner (cargo build --release)
#
# Safety: This script ONLY deploys to TESTNET Azure VMs.
#         Mainnet (Hetzner) is never touched.

HETZNER="andrey@94.130.18.162"
BINARY_PATH="$HOME/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator"
REMOTE_DIR="/home/azureuser/perp"

declare -A NODES=(
    [node-1]="20.71.184.176"
    [node-2]="48.209.17.158"
    [node-3]="74.234.43.107"
)

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log()  { echo -e "${GREEN}[deploy]${NC} $*"; }
warn() { echo -e "${YELLOW}[warn]${NC} $*"; }
err()  { echo -e "${RED}[error]${NC} $*" >&2; }

# ── Pre-flight checks ──────────────────────────────────────────

preflight() {
    log "Pre-flight checks..."

    # 1. Mainnet still running on Hetzner?
    local mainnet_pid
    mainnet_pid=$(ssh "$HETZNER" 'pgrep -f "perp-dex-orchestrator.*mainnet" 2>/dev/null || echo "none"')
    if [ "$mainnet_pid" = "none" ]; then
        warn "No mainnet orchestrator detected on Hetzner (may be expected)"
    else
        log "Mainnet orchestrator running (PID $mainnet_pid) — will not touch"
    fi

    # 2. Binary exists?
    if ! ssh "$HETZNER" "test -f $BINARY_PATH"; then
        err "Binary not found at $BINARY_PATH on Hetzner"
        err "Run: ssh $HETZNER 'source ~/.cargo/env && cd ~/llm-perp-xrpl/orchestrator && cargo build --release'"
        exit 1
    fi

    # 3. Binary version
    local version
    version=$(ssh "$HETZNER" "$BINARY_PATH --version 2>/dev/null || echo unknown")
    log "Binary version: $version"
}

# ── Deploy to a single node ────────────────────────────────────

deploy_node() {
    local name="$1"
    local ip="${NODES[$name]}"

    log "[$name] Deploying to $ip..."

    # Stop old process
    log "[$name] Stopping old process..."
    ssh "$HETZNER" "ssh -o StrictHostKeyChecking=no azureuser@$ip 'pkill -f perp-dex-orchestrator || true'" || true

    sleep 2

    # Copy binary via Hetzner bastion
    log "[$name] Copying binary..."
    ssh "$HETZNER" "scp -o StrictHostKeyChecking=no $BINARY_PATH azureuser@$ip:$REMOTE_DIR/perp-dex-orchestrator.new"

    # Atomic swap
    log "[$name] Swapping binary..."
    ssh "$HETZNER" "ssh azureuser@$ip 'cd $REMOTE_DIR && mv perp-dex-orchestrator.new perp-dex-orchestrator && chmod +x perp-dex-orchestrator'"

    # Start (use systemd if available, otherwise nohup)
    log "[$name] Starting..."
    ssh "$HETZNER" "ssh azureuser@$ip 'sudo systemctl restart perp-dex-orchestrator 2>/dev/null || (cd $REMOTE_DIR && nohup ./start.sh > orchestrator.log 2>&1 &)'"

    sleep 3

    # Health check
    log "[$name] Health check..."
    local health
    health=$(ssh "$HETZNER" "ssh azureuser@$ip 'curl -s http://localhost:3000/v1/health'" 2>/dev/null || echo '{"status":"unreachable"}')
    log "[$name] $health"

    if echo "$health" | grep -q '"status":"ok"'; then
        log "[$name] ${GREEN}OK${NC}"
    else
        warn "[$name] Health check did not return ok — check logs"
    fi
}

# ── Main ───────────────────────────────────────────────────────

target="${1:-all}"

preflight

if [ "$target" = "all" ]; then
    for node in node-1 node-2 node-3; do
        deploy_node "$node"
    done
else
    if [[ ! -v "NODES[$target]" ]]; then
        err "Unknown node: $target (expected node-1, node-2, node-3, or all)"
        exit 1
    fi
    deploy_node "$target"
fi

log "Deploy complete. Waiting 10s for P2P mesh to form..."
sleep 10

# Final health check on all deployed nodes
log "Final health status:"
for node in "${!NODES[@]}"; do
    ip="${NODES[$node]}"
    health=$(ssh "$HETZNER" "ssh azureuser@$ip 'curl -s http://localhost:3000/v1/health'" 2>/dev/null || echo "unreachable")
    log "  $node ($ip): $health"
done
