#!/usr/bin/env bash
#
# Self-test for scripts/ci/check-go-floor-agreement.sh (issue #4852).
#
# The gate reads Dockerfiles from a root, so every case here is a scratch tree
# with hand-written `ARG GO_MIN_PATCH=` lines. No network, no docker, ~1s.
#
# The cases that matter are the ones that make the gate a gate rather than a
# formality: a split floor MUST fail (that is #4852's class), and a tree with
# NO floor must be INFRA rather than a pass (otherwise renaming the ARG turns
# the gate off silently).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$HERE/check-go-floor-agreement.sh"

pass=0
fail=0

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

# expect <label> <expected-exit> <root> [<stdout-needle>]
expect() {
  local label="$1" want="$2" root="$3" needle="${4:-}"
  local out rc
  out="$(bash "$GATE" "$root" 2>&1)"
  rc=$?

  if [[ "$rc" != "$want" ]]; then
    printf '  \033[31mFAIL\033[0m %s: exit %s, wanted %s\n' "$label" "$rc" "$want"
    printf '%s\n' "$out" | sed 's/^/        /'
    fail=$((fail + 1))
    return
  fi

  if [[ -n "$needle" ]] && ! grep -qF -- "$needle" <<< "$out"; then
    printf '  \033[31mFAIL\033[0m %s: exit %s ok but output lacks %q\n' "$label" "$rc" "$needle"
    printf '%s\n' "$out" | sed 's/^/        /'
    fail=$((fail + 1))
    return
  fi

  printf '  ok   %s\n' "$label"
  pass=$((pass + 1))
}

mktree() { # <name> -> echoes the root
  local d="$tmp/$1"
  mkdir -p "$d/docker"
  echo "$d"
}

# --- 1. all floors equal -> pass -------------------------------------------
r="$(mktree agree)"
{
  echo 'FROM golang:1.27-alpine'
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.backend"
{
  echo 'FROM golang:1.27-alpine'
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.backend.alpine"
{
  echo 'FROM golang:1.27-alpine'
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.scanner-adapter"
expect "three equal floors -> OK" 0 "$r" 'All 3 GO_MIN_PATCH floors agree on 1.27.3'

# --- 2. a split floor -> FAIL (#4852's class) ------------------------------
r="$(mktree split)"
{
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.backend"
{
  echo 'ARG GO_MIN_PATCH=1.27.2'
} > "$r/docker/Dockerfile.scanner-adapter"
expect "one floor behind -> FAIL" 1 "$r" 'Go toolchain floors disagree'

# --- 3. a floor that is not a version -> FAIL ------------------------------
r="$(mktree malformed)"
{
  echo 'ARG GO_MIN_PATCH=1.27'
} > "$r/docker/Dockerfile.backend"
expect "non-X.Y.Z floor -> FAIL" 1 "$r" 'not a X.Y.Z version'

# --- 4. no floor at all -> INFRA, not a pass -------------------------------
r="$(mktree none)"
echo 'FROM golang:1.27-alpine' > "$r/docker/Dockerfile.backend"
expect "no floor present -> INFRA" 2 "$r" 'found no'

# --- 5. a commented-out floor is not a floor -------------------------------
r="$(mktree commented)"
{
  echo '# ARG GO_MIN_PATCH=1.27.2  (retired)'
} > "$r/docker/Dockerfile.backend"
{
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.scanner-adapter"
expect "commented floor ignored -> OK" 0 "$r" 'All 1 GO_MIN_PATCH floors agree on 1.27.3'

# --- 6. a trailing comment on the ARG line is tolerated --------------------
r="$(mktree trailing)"
{
  echo 'ARG GO_MIN_PATCH=1.27.3 # keep in lockstep'
} > "$r/docker/Dockerfile.backend"
expect "trailing comment parsed -> OK" 0 "$r" 'All 1 GO_MIN_PATCH floors agree on 1.27.3'

# --- 7. leading whitespace is still a floor --------------------------------
r="$(mktree indented)"
{
  printf '    ARG GO_MIN_PATCH=1.27.3\n'
} > "$r/docker/Dockerfile.backend"
expect "indented ARG is recognised -> OK" 0 "$r" 'All 1 GO_MIN_PATCH floors agree on 1.27.3'

# --- 8. no docker/ directory -> INFRA --------------------------------------
r="$tmp/nodocker"; mkdir -p "$r"
expect "no docker/ dir -> INFRA" 2 "$r" 'no docker/ directory'

# --- 9. a path containing a dash is printed intact -------------------------
# The first cut of this gate split the grep location on the last `-`, which
# mangled `Dockerfile.scanner-adapter` into `Dockerfile.scanner:adapter` in the
# report. The report is what a human acts on, so a corrupted path is a defect
# even when the verdict is right. This keeps it fixed.
r="$(mktree dashy)"
{
  echo 'ARG GO_MIN_PATCH=1.27.3'
} > "$r/docker/Dockerfile.backend-alpine-x"
expect "path with dashes printed intact" 0 "$r" 'Dockerfile.backend-alpine-x:1'

# --- 9. the REAL repository is consistent right now ------------------------
# The gate's whole point is that it is green on the tree it ships with; a case
# that only proves it fails would not notice it failing on main.
repo="$(cd "$HERE/../.." && pwd)"
if [[ -d "$repo/docker" ]]; then
  expect "real repository floors agree" 0 "$repo" 'GO_MIN_PATCH floors agree on'
else
  echo "  -\033[33mSKIP\033[0m real repository: no docker/ under $repo"
fi

echo
if [[ $fail -gt 0 ]]; then
  echo "FAILED: $fail/$((pass + fail)) cases"
  exit 1
fi
echo "all check-go-floor-agreement cases passed ($pass)"
