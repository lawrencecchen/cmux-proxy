#!/usr/bin/env bash
set -euo pipefail

# End-to-end tests for cmux-proxy using Docker and LD_PRELOAD isolation.
# - Builds the runtime image
# - Starts proxy container publishing :8080
# - Inside the container, starts HTTP servers bound by workspace via LD_PRELOAD
# - Verifies isolation from inside the container
# - Verifies proxy from host via headers and subdomain Host routing

PORT="${PORT:-8080}"
IMAGE="${IMAGE:-cmux-proxy-e2e:latest}"
CONTAINER="${CONTAINER:-cmux-proxy-e2e}"

WS_A="workspace-a"
WS_B="workspace-b"

red() { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || { red "Missing required command: $1"; exit 1; }
}

require_cmd docker
require_cmd curl

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "[1/8] Building runtime image: $IMAGE"
docker build --target runtime -t "$IMAGE" .

echo "[2/8] Starting proxy container: $CONTAINER (publishing :$PORT)"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --rm \
  -p "$PORT:8080" \
  --name "$CONTAINER" \
  "$IMAGE" >/dev/null

echo "[3/8] Waiting for proxy to listen on :$PORT"
for i in $(seq 1 50); do
  code=$(curl -sS -o /dev/null -w "%{http_code}" "http://127.0.0.1:${PORT}/" || true)
  # No header -> expect 400 once proxy is up
  if [ "$code" = "400" ]; then break; fi
  sleep 0.1
done
code=$(curl -sS -o /dev/null -w "%{http_code}" "http://127.0.0.1:${PORT}/" || true)
if [ "$code" != "400" ]; then
  red "Proxy did not start (expected HTTP 400 without headers). Got: $code"
  exit 1
fi
green "Proxy is up."

echo "[4/8] Prepare workspaces and start server in $WS_A on port 3000"
docker exec "$CONTAINER" bash -lc "mkdir -p /root/$WS_A /root/$WS_B"
docker exec "$CONTAINER" bash -lc "cd /root/$WS_A && echo ok-A > index.html && nohup python3 -m http.server 3000 --bind 127.0.0.1 >/tmp/a.log 2>&1 & echo \$! > /tmp/a.pid"

echo "Waiting for A:3000 inside container"
for i in $(seq 1 50); do
  if docker exec "$CONTAINER" bash -lc "cd /root/$WS_A && curl -sS --fail http://127.0.0.1:3000 >/dev/null"; then
    break
  fi
  sleep 0.1
done
docker exec "$CONTAINER" bash -lc "cd /root/$WS_A && curl -sS --fail http://127.0.0.1:3000 | grep -q '^ok-A$'"
green "A:3000 is serving index.html"

echo "[5/8] Verify isolation inside container (B cannot reach A's 3000)"
set +e
docker exec "$CONTAINER" bash -lc "cd /root/$WS_B && curl -sS -m 2 http://127.0.0.1:3000 >/dev/null"
status=$?
set -e
if [ $status -eq 0 ]; then
  red "Isolation failed: curl from $WS_B to 127.0.0.1:3000 succeeded (should fail)"
  exit 1
fi
green "Isolation inside container OK (B cannot reach A:3000)."

echo "[6/8] Validate proxy from host using headers"
body=$(curl -sS -H "X-Cmux-Workspace-Internal: $WS_A" -H "X-Cmux-Port-Internal: 3000" "http://127.0.0.1:${PORT}/")
test "$body" = "ok-A" || { red "Expected 'ok-A' via headers (A), got: $body"; exit 1; }
code=$(curl -sS -o /dev/null -w "%{http_code}" -H "X-Cmux-Workspace-Internal: $WS_B" -H "X-Cmux-Port-Internal: 3000" "http://127.0.0.1:${PORT}/")
test "$code" = "502" || { red "Expected 502 via headers (B w/o server), got: $code"; exit 1; }
green "Header routing OK (A success, B 502)."

echo "[7/8] Validate proxy from host using subdomain Host header"
body=$(curl -sS -H "Host: ${WS_A}-3000.localhost" "http://127.0.0.1:${PORT}/")
test "$body" = "ok-A" || { red "Expected 'ok-A' via subdomain (A), got: $body"; exit 1; }
code=$(curl -sS -o /dev/null -w "%{http_code}" -H "Host: ${WS_B}-3000.localhost" "http://127.0.0.1:${PORT}/")
test "$code" = "502" || { red "Expected 502 via subdomain (B w/o server), got: $code"; exit 1; }
green "Subdomain routing OK (A success, B 502)."

echo "Starting server in $WS_B on port 3000"
docker exec "$CONTAINER" bash -lc "cd /root/$WS_B && echo ok-B > index.html && nohup python3 -m http.server 3000 --bind 127.0.0.1 >/tmp/b.log 2>&1 & echo \$! > /tmp/b.pid"
echo "Waiting for B:3000 inside container"
for i in $(seq 1 50); do
  if docker exec "$CONTAINER" bash -lc "cd /root/$WS_B && curl -sS --fail http://127.0.0.1:3000 >/dev/null"; then
    break
  fi
  sleep 0.1
done

echo "[8/8] Re-validate proxy for B now that server is up"
body=$(curl -sS -H "X-Cmux-Workspace-Internal: $WS_B" -H "X-Cmux-Port-Internal: 3000" "http://127.0.0.1:${PORT}/")
test "$body" = "ok-B" || { red "Expected 'ok-B' via headers (B), got: $body"; exit 1; }
body=$(curl -sS -H "Host: ${WS_B}-3000.localhost" "http://127.0.0.1:${PORT}/")
test "$body" = "ok-B" || { red "Expected 'ok-B' via subdomain (B), got: $body"; exit 1; }

# Additional validations
echo "[9/9] Validate two servers on same port in different workspaces"
docker exec "$CONTAINER" bash -lc "cd /root/$WS_A && curl -sS --fail http://127.0.0.1:3000 | grep -q '^ok-A$'"
docker exec "$CONTAINER" bash -lc "cd /root/$WS_B && curl -sS --fail http://127.0.0.1:3000 | grep -q '^ok-B$'"
green "A:3000 and B:3000 both serve their own content (no conflict)."

echo "[9/9] Validate two servers on same port in the SAME workspace fail"
docker exec "$CONTAINER" bash -lc "mkdir -p /root/workspace-c && echo ok-C > /root/workspace-c/index.html && cd /root/workspace-c && nohup python3 -m http.server 3000 --bind 127.0.0.1 >/tmp/c1.log 2>&1 & echo \$! > /tmp/c1.pid"
for i in $(seq 1 50); do
  if docker exec "$CONTAINER" bash -lc "cd /root/workspace-c && curl -sS --fail http://127.0.0.1:3000 >/dev/null"; then
    break
  fi
  sleep 0.1
done

set +e
out=$(docker exec "$CONTAINER" bash -lc "cd /root/workspace-c && python3 -m http.server 3000 --bind 127.0.0.1" 2>&1)
rc=$?
set -e
if [ $rc -eq 0 ]; then
  red "Second server in same workspace unexpectedly succeeded"
  echo "$out"
  exit 1
fi
echo "$out" | grep -qi "address already in use" || { red "Expected 'address already in use' in error, got:\n$out"; exit 1; }
green "Same-workspace second bind failed with 'address already in use' as expected."

green "All e2e tests passed."
