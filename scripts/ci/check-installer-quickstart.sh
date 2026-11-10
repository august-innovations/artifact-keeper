#!/usr/bin/env bash
#
# Regression gate for #4313: what a first-time user gets from the quickstart
# installer (scripts/install.sh) must match the compose file and the docs.
#
# Runs the real installer with AK_SKIP_START=1 in a scratch directory, with
# `curl` served from this checkout and stub `docker`/`podman` binaries, and
# checks the generated .env:
#   - HTTP_PORT/HTTPS_PORT default to the compose file's own defaults
#     (30080/30443), which the docs use and rootless Podman can publish;
#   - DEPENDENCY_TRACK_ENABLED=true only together with COMPOSE_PROFILES=dtrack
#     (the integration flag without the profile pointed the backend at a
#     service that never starts);
#   - AK_ADMIN_PASSWORD given to `bash` lands in ADMIN_PASSWORD;
#   - a docker CLI whose daemon is unreachable falls back to Podman.
#
# Usage: check-installer-quickstart.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILED=0
fail() { echo "FAIL: $*" >&2; FAILED=1; }

mkdir -p "$WORK/bin"
# curl -fsSL <REPO_BASE>/<path> -o <dest>: copy <path> from this checkout.
cat > "$WORK/bin/curl" <<STUB
#!/usr/bin/env bash
url=""; out=""
while [ \$# -gt 0 ]; do
  case "\$1" in -o) out="\$2"; shift 2 ;; -*) shift ;; *) url="\$1"; shift ;; esac
done
src="$ROOT/\${url#*/artifact-keeper/main/}"
[ -n "\$out" ] && cp "\$src" "\$out" || cat "\$src"
STUB
# docker: compose plugin present, daemon unreachable (`docker info` fails).
cat > "$WORK/bin/docker" <<'STUB'
#!/usr/bin/env bash
[ "$1 $2" = "compose version" ] && exit 0
exit 1
STUB
cat > "$WORK/bin/podman" <<'STUB'
#!/usr/bin/env bash
[ "$1 $2" = "compose version" ] && exit 0
exit 0
STUB
chmod +x "$WORK/bin/"*

run() { # run <name> [VAR=value ...]
  local name="$1"; shift
  env PATH="$WORK/bin:/usr/bin:/bin" INSTALL_DIR="$WORK/$name" AK_SKIP_START=1 "$@" \
    bash "$ROOT/scripts/install.sh" > "$WORK/$name.log" 2>&1 \
    || { fail "$name: installer exited non-zero"; cat "$WORK/$name.log" >&2; return; }
}
envval() { sed -n "s/^$2=//p" "$WORK/$1/.env" | tail -1; }

run default
if [ -f "$WORK/default/.env" ]; then
  [ "$(envval default HTTP_PORT)" = "30080" ] || fail "default HTTP_PORT is $(envval default HTTP_PORT), compose defaults to 30080"
  [ "$(envval default HTTPS_PORT)" = "30443" ] || fail "default HTTPS_PORT is $(envval default HTTPS_PORT), compose defaults to 30443"
  grep -q '"${HTTP_PORT:-30080}:80"' "$ROOT/docker-compose.yml" || fail "docker-compose.yml no longer defaults HTTP_PORT to 30080; update the installer and this check"
  grep -q 'Found Podman' "$WORK/default.log" || fail "docker without a reachable daemon did not fall back to Podman"
fi

for name in default dtrack; do
  [ "$name" = dtrack ] && run dtrack AK_DTRACK=1
  [ -f "$WORK/$name/.env" ] || continue
  if [ "$(envval "$name" DEPENDENCY_TRACK_ENABLED)" = "true" ] \
     && ! grep -q '^COMPOSE_PROFILES=.*dtrack' "$WORK/$name/.env"; then
    fail "$name: DEPENDENCY_TRACK_ENABLED=true without COMPOSE_PROFILES=dtrack"
  fi
done
[ "$(envval dtrack DEPENDENCY_TRACK_ENABLED)" = "true" ] || fail "AK_DTRACK=1 did not enable Dependency-Track"

run preset AK_ADMIN_PASSWORD='Preset-4313-Passw0rd'
[ "$(envval preset ADMIN_PASSWORD)" = "Preset-4313-Passw0rd" ] || fail "AK_ADMIN_PASSWORD was not written to ADMIN_PASSWORD"

[ "$FAILED" = 0 ] && echo "OK: the quickstart installer matches the compose defaults and docs"
exit "$FAILED"
