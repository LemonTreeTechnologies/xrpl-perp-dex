#!/usr/bin/env bash
set -euo pipefail

# Deploy perp-dex-orchestrator to Azure testnet cluster.
#
# Usage:
#   ./scripts/deploy.sh [node-1|node-2|node-3|all]
#   ./scripts/deploy.sh rollback [node-1|node-2|node-3|all]
#
# Prerequisites:
#   - SSH access via Hetzner bastion (94.130.18.162)
#   - Binary built on Hetzner (cargo build --release)
#
# Safety: This script ONLY deploys to TESTNET Azure VMs.
#         Mainnet (Hetzner) is never touched.

BINARY_PATH="$HOME/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator"
REMOTE_DIR="/home/azureuser/perp"

declare -A NODES=(
    [node-1]="20.71.184.176"
    [node-2]="20.224.243.60"
    [node-3]="52.236.130.102"
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

    if [ ! -f "$BINARY_PATH" ]; then
        err "Binary not found at $BINARY_PATH"
        err "Run: source ~/.cargo/env && cd ~/llm-perp-xrpl/orchestrator && cargo build --release"
        exit 1
    fi

    local version
    version=$("$BINARY_PATH" --version 2>/dev/null || echo "unknown")
    log "Binary version: $version"
    log "Binary sha256: $(sha256sum "$BINARY_PATH" | cut -d' ' -f1 | head -c 16)..."
}

# ── Health check helper ───────────────────────────────────────

health_check() {
    local name="$1" ip="$2"
    local health
    health=$(ssh -o ConnectTimeout=5 azureuser@"$ip" \
        'curl -s --connect-timeout 5 --max-time 10 http://localhost:3000/v1/health' 2>/dev/null \
        || echo '{"status":"unreachable"}')
    echo "$health"
}

# ── Deploy to a single node ────────────────────────────────────

deploy_node() {
    local name="$1"
    local ip="${NODES[$name]}"

    log "[$name] Deploying to $ip..."

    # Save current version before deploy
    local old_version
    old_version=$(ssh -o ConnectTimeout=5 azureuser@"$ip" \
        "cd $REMOTE_DIR && ./perp-dex-orchestrator --version 2>/dev/null || echo unknown" \
        2>/dev/null || echo "unknown")
    log "[$name] Current version: $old_version"

    # Backup current binary for rollback
    log "[$name] Backing up current binary..."
    ssh -o StrictHostKeyChecking=no azureuser@"$ip" \
        "cd $REMOTE_DIR && cp -f perp-dex-orchestrator perp-dex-orchestrator.prev 2>/dev/null || true"

    # Stop old process
    log "[$name] Stopping old process..."
    ssh azureuser@"$ip" 'sudo systemctl stop perp-dex-orchestrator 2>/dev/null; killall perp-dex-orchestrator 2>/dev/null; echo stopped' || true
    sleep 2

    # Copy binary
    log "[$name] Copying binary..."
    scp -o StrictHostKeyChecking=no "$BINARY_PATH" azureuser@"$ip":"$REMOTE_DIR"/perp-dex-orchestrator.new

    # Atomic swap
    log "[$name] Swapping binary..."
    ssh azureuser@"$ip" "cd $REMOTE_DIR && mv perp-dex-orchestrator.new perp-dex-orchestrator && chmod +x perp-dex-orchestrator"

    # Start
    log "[$name] Starting..."
    ssh azureuser@"$ip" "sudo systemctl restart perp-dex-orchestrator 2>/dev/null || (cd $REMOTE_DIR && nohup ./start_orchestrator.sh </dev/null > orchestrator.log 2>&1 & echo PID=\$!)"
    sleep 3

    # Health check
    log "[$name] Health check..."
    local health
    health=$(health_check "$name" "$ip")
    log "[$name] $health"

    if echo "$health" | grep -q '"status":"ok"'; then
        log "[$name] ${GREEN}OK${NC}"
        # Log deployment
        ssh azureuser@"$ip" "echo '$(date -u +%Y-%m-%dT%H:%M:%SZ) deployed from $old_version' >> $REMOTE_DIR/deploy.log" 2>/dev/null || true
    else
        warn "[$name] Health check failed — rolling back..."
        rollback_node "$name"
        return 1
    fi
}

# ── Rollback a single node ────────────────────────────────────

rollback_node() {
    local name="$1"
    local ip="${NODES[$name]}"

    log "[$name] Rolling back on $ip..."

    # Check backup exists
    local has_prev
    has_prev=$(ssh azureuser@"$ip" "test -f $REMOTE_DIR/perp-dex-orchestrator.prev && echo yes || echo no" 2>/dev/null)
    if [ "$has_prev" != "yes" ]; then
        err "[$name] No backup binary found — cannot rollback"
        return 1
    fi

    # Stop current
    ssh azureuser@"$ip" 'sudo systemctl stop perp-dex-orchestrator 2>/dev/null; killall perp-dex-orchestrator 2>/dev/null' || true
    sleep 2

    # Restore backup
    ssh azureuser@"$ip" "cd $REMOTE_DIR && mv perp-dex-orchestrator.prev perp-dex-orchestrator && chmod +x perp-dex-orchestrator"

    # Start
    ssh azureuser@"$ip" "sudo systemctl restart perp-dex-orchestrator 2>/dev/null || (cd $REMOTE_DIR && nohup ./start_orchestrator.sh </dev/null > orchestrator.log 2>&1 & echo PID=\$!)"
    sleep 3

    # Verify
    local health
    health=$(health_check "$name" "$ip")
    log "[$name] After rollback: $health"

    if echo "$health" | grep -q '"status":"ok"'; then
        log "[$name] ${GREEN}Rollback OK${NC}"
        ssh azureuser@"$ip" "echo '$(date -u +%Y-%m-%dT%H:%M:%SZ) ROLLBACK' >> $REMOTE_DIR/deploy.log" 2>/dev/null || true
    else
        err "[$name] Rollback also failed — manual intervention needed"
        return 1
    fi
}

# ── Main ───────────────────────────────────────────────────────

action="${1:-deploy}"
target="${2:-all}"

# Handle "deploy rollback" syntax
if [ "$action" = "rollback" ]; then
    if [ "$target" = "all" ]; then
        for node in node-1 node-2 node-3; do
            rollback_node "$node"
        done
    else
        if [[ ! -v "NODES[$target]" ]]; then
            err "Unknown node: $target (expected node-1, node-2, node-3, or all)"
            exit 1
        fi
        rollback_node "$target"
    fi
    exit 0
fi

# Regular deploy (first arg is target if not "rollback")
target="$action"
[ "$target" = "deploy" ] && target="${2:-all}"

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
for node in node-1 node-2 node-3; do
    ip="${NODES[$node]}"
    health=$(health_check "$node" "$ip")
    log "  $node ($ip): $health"
done
