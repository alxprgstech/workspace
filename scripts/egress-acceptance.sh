#!/usr/bin/env bash
# Practical acceptance for the trusted egress proxy (stage 013).
#
# Run inside the dedicated runtime as the workspace user:
#
#   scripts/egress-acceptance.sh <client-image>
#
# The client image must contain curl (the capability base image does). The
# script proves that:
#   1. a container on the workspace network cannot reach the internet directly;
#   2. an allowlisted host is reachable through the egress proxy;
#   3. a host outside the allowlist is denied;
#   4. plain HTTP and the metadata address are denied.
set -euo pipefail

docker() {
    command /usr/bin/docker --host "unix:///run/user/1000/docker.sock" "$@"
}

IMAGE="${1:-}"
DATA_ROOT="${DATA_ROOT:-/srv/alxprgs-workspace/data}"
BINARY="${EGRESS_BINARY:-/srv/alxprgs-workspace/bin/workspace-egress}"
NET_IN="alxprgs-egress-check-int"
NET_OUT="alxprgs-egress-check-out"
PROXY="alxprgs-egress-check"
ALLOWLIST="$DATA_ROOT/egress-acceptance-allow.txt"

if [[ -z "$IMAGE" ]]; then
    echo "usage: $0 <client-image-with-curl>" >&2
    exit 2
fi

if [[ ! -x "$BINARY" ]]; then
    echo "missing executable egress proxy: $BINARY" >&2
    exit 2
fi

cleanup() {
    docker rm -f "$PROXY" >/dev/null 2>&1 || true
    docker network rm "$NET_IN" >/dev/null 2>&1 || true
    docker network rm "$NET_OUT" >/dev/null 2>&1 || true
    rm -f "$ALLOWLIST"
}
trap cleanup EXIT

printf 'example.com\ncrates.io\nstatic.crates.io\nindex.crates.io\n' > "$ALLOWLIST"
chmod 0644 "$ALLOWLIST"

docker network create "$NET_OUT" >/dev/null
docker network create --internal "$NET_IN" >/dev/null

docker run -d --name "$PROXY" \
    --network "$NET_OUT" \
    --read-only --cap-drop ALL --security-opt no-new-privileges \
    --mount "type=bind,source=$BINARY,target=/egress,readonly" \
    --mount "type=bind,source=$ALLOWLIST,target=/etc/egress/allowlist,readonly" \
    --env EGRESS_ALLOWLIST=/etc/egress/allowlist \
    "$IMAGE" /egress >/dev/null

docker network connect "$NET_IN" "$PROXY"

sleep 1

script=$(cat <<'EOF'
set -u
direct=$(curl -sS --max-time 8 -o /dev/null -w '%{http_code}' https://example.com 2>/dev/null || true)
allowed=$(curl -sS --max-time 15 -x http://alxprgs-egress-check:3128 -o /dev/null -w '%{http_code}' https://example.com 2>/dev/null || true)
denied=$(curl -sS --max-time 10 -x http://alxprgs-egress-check:3128 -o /dev/null -w '%{http_code}' https://example.org 2>/dev/null || true)
metadata=$(curl -sS --max-time 10 -x http://alxprgs-egress-check:3128 -o /dev/null -w '%{http_code}' http://169.254.169.254/ 2>/dev/null || true)
printf 'direct=%s allowed=%s denied=%s metadata=%s\n' "$direct" "$allowed" "$denied" "$metadata"
EOF
)

result=$(docker run --rm --network "$NET_IN" \
    --read-only --cap-drop ALL --security-opt no-new-privileges \
    --tmpfs /tmp:rw,size=8m \
    "$IMAGE" /bin/bash -c "$script")

echo "$result"

fail=0

case "$result" in
    *"direct=000"*) ;;
    *) echo "FAIL: direct egress was not blocked" >&2; fail=1 ;;
esac

case "$result" in
    *"allowed=200"*) ;;
    *) echo "FAIL: allowlisted host was not reachable through the proxy" >&2; fail=1 ;;
esac

case "$result" in
    *"denied=000"*) ;;
    *) echo "FAIL: host outside the allowlist was not denied" >&2; fail=1 ;;
esac

case "$result" in
    *"metadata=405"*) ;;
    *) echo "FAIL: plain-HTTP metadata access was not denied" >&2; fail=1 ;;
esac

if [[ "$fail" -ne 0 ]]; then
    docker logs "$PROXY" >&2 || true
    exit 1
fi

echo "egress acceptance PASSED"
