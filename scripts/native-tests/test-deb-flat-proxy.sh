#!/bin/bash
# Debian flat-repository proxy E2E test (#4619)
#
# A flat APT repository (`deb <uri> /`) has no dists/<suite> tree: InRelease,
# Packages and the .deb files sit under the URI itself. pkgs.k8s.io publishes
# Kubernetes this way. This test points a Debian remote (and a virtual repo
# over it) at such an upstream and drives the real apt client through it:
# `apt-get update` must verify the publisher's signed InRelease and
# `apt-get install --download-only` must fetch the package.
set -euo pipefail

REGISTRY_URL="${REGISTRY_URL:-http://localhost:8080}"
FLAT_REPO_KEY="${FLAT_REPO_KEY:-deb-flat-proxy}"
FLAT_VIRTUAL_KEY="${FLAT_VIRTUAL_KEY:-deb-flat-virtual}"
FLAT_UPSTREAM_URL="${FLAT_UPSTREAM_URL:-https://pkgs.k8s.io/core:/stable:/v1.34/deb/}"
FLAT_PACKAGE="${FLAT_PACKAGE:-kubectl}"
ADMIN_USER="${ADMIN_USER:-admin}"
# Throwaway e2e admin credential: the value is defined once, in the
# repository-root .env.test (#3490). Absent inside an e2e container, where
# compose has already injected the same variables from the same file.
_ak_test_env="$(dirname "$0")/../lib/test-env.sh"
# shellcheck source=/dev/null
[ -r "$_ak_test_env" ] && . "$_ak_test_env"
ADMIN_PASS="${ADMIN_PASS:-${AK_TEST_ADMIN_PASSWORD:-}}"

echo "==> Debian Flat Repository Proxy E2E Test (#4619)"
echo "Registry: $REGISTRY_URL"
echo "Upstream: $FLAT_UPSTREAM_URL"
echo ""

if ! command -v curl >/dev/null 2>&1; then
  apt-get update -qq
  apt-get install -y -qq curl ca-certificates >/dev/null
fi

api() {
  curl -s -u "$ADMIN_USER:$ADMIN_PASS" -H "Content-Type: application/json" "$@"
}

ensure_repo() { # ensure_repo <key> <json>
  local code
  code=$(api -o /dev/null -w "%{http_code}" "$REGISTRY_URL/api/v1/repositories/$1")
  if [ "$code" = "404" ]; then
    code=$(api -o /dev/null -w "%{http_code}" -X POST "$REGISTRY_URL/api/v1/repositories" -d "$2")
    if [ "$code" -ge 300 ]; then
      echo "❌ Failed to create repository $1 (HTTP $code)"
      exit 1
    fi
    echo "  Created $1"
  else
    echo "  $1 already exists (HTTP $code)"
  fi
}

# ---- Step 1: remote over the flat upstream, and a virtual over the remote ----
echo "==> [1/4] Creating the remote and virtual repositories..."
ensure_repo "$FLAT_REPO_KEY" "{
  \"key\": \"$FLAT_REPO_KEY\", \"name\": \"Debian flat proxy\", \"format\": \"debian\",
  \"repo_type\": \"remote\", \"upstream_url\": \"$FLAT_UPSTREAM_URL\", \"is_public\": true
}"
ensure_repo "$FLAT_VIRTUAL_KEY" "{
  \"key\": \"$FLAT_VIRTUAL_KEY\", \"name\": \"Debian flat virtual\", \"format\": \"debian\",
  \"repo_type\": \"virtual\", \"is_public\": true
}"
api -o /dev/null -X POST "$REGISTRY_URL/api/v1/repositories/$FLAT_VIRTUAL_KEY/members" \
  -d "{\"member_key\": \"$FLAT_REPO_KEY\", \"priority\": 1}" || true

# ---- Step 2: trust the publisher's key, exactly as the upstream documents ----
echo ""
echo "==> [2/4] Installing the publisher's signing key..."
mkdir -p /etc/apt/keyrings
curl -fsSL "${FLAT_UPSTREAM_URL%/}/Release.key" -o /etc/apt/keyrings/flat-upstream.asc

apt_through() { # apt_through <repo-key>
  rm -f /etc/apt/sources.list.d/ak-flat.list
  echo "deb [signed-by=/etc/apt/keyrings/flat-upstream.asc] $REGISTRY_URL/debian/$1/ /" \
    > /etc/apt/sources.list.d/ak-flat.list
  apt-get clean
  apt-get update -o Dir::Etc::sourcelist=/dev/null \
    -o Dir::Etc::sourceparts=/etc/apt/sources.list.d 2>&1 | tee /tmp/ak-flat-update.log
  if grep -qE '^(E|Err):' /tmp/ak-flat-update.log; then
    echo "❌ apt-get update through $1 failed"
    exit 1
  fi
  apt-get install -y --no-install-recommends --download-only "$FLAT_PACKAGE"
  if ! ls /var/cache/apt/archives/"${FLAT_PACKAGE}"_*.deb >/dev/null 2>&1; then
    echo "❌ $FLAT_PACKAGE was not downloaded through $1"
    exit 1
  fi
  echo "  ✅ $FLAT_PACKAGE downloaded through $1"
}

# ---- Step 3: apt through the remote, then through the virtual ----
echo ""
echo "==> [3/4] apt-get update / install --download-only through the proxy..."
apt_through "$FLAT_REPO_KEY"
apt_through "$FLAT_VIRTUAL_KEY"

# ---- Step 4: the suite form names the flat form instead of a bare 502 ----
echo ""
echo "==> [4/4] Checking the dists/ form answers with the flat sources hint..."
RESP=$(curl -s -w "\n%{http_code}" "$REGISTRY_URL/debian/$FLAT_REPO_KEY/dists/stable/InRelease")
CODE=$(echo "$RESP" | tail -1)
if [ "$CODE" != "404" ] || ! echo "$RESP" | grep -q "flat"; then
  echo "❌ Expected 404 with the flat sources hint, got HTTP $CODE"
  exit 1
fi
echo "  ✅ dists/stable/InRelease -> 404 with the flat sources line"

rm -f /etc/apt/sources.list.d/ak-flat.list
echo ""
echo "✅ Debian flat repository proxy test PASSED"
