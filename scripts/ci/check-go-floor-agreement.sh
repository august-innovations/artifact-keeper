#!/usr/bin/env bash
#
# CI gate: every Dockerfile's Go toolchain floor names the SAME version.
#
# WHY THIS GATE EXISTS (#4852)
# ----------------------------
# The Go stdlib is compiled INTO every binary we build with Go, so a stdlib CVE
# is a property of the toolchain and not of the image that happens to be
# reported first. The pinned-base-image watch (#3755) scans the digest-pinned
# bases -- the scanner adapter's bundled trivy is one -- and reports a finding
# against whichever image it scans. The obvious response is to raise the floor
# in THAT Dockerfile and stop, and that is exactly the wrong repair: the same
# stdlib is in grype and buildctl in the backend, on a floor nobody re-read
# because no report named it.
#
# #4852 is that shape. The watch named stdlib v1.27.2 in the adapter's trivy;
# `GO_MIN_PATCH=1.27.2` was sitting in three Dockerfiles, and only a human
# noticing all three is what fixes the class rather than the one image.
#
# So the invariant is: `GO_MIN_PATCH` is one number, written in several files.
# Disagreement is either a floor about to go stale (an image that did not get
# the bump) or a deliberate split -- and a deliberate split is NOT supported:
# the floors guard the SAME Go release line on the SAME floating
# `golang:1.27-alpine` tag, so a difference can only mean one of them is
# behind. If a real split is ever wanted, this gate is the place that says so,
# with the reason, rather than a silent drift.
#
# WHAT IT CHECKS
#   1. Every `ARG GO_MIN_PATCH=<X.Y.Z>` in docker/Dockerfile* names the same
#      version. A malformed value is a failure, not a skip.
#   2. There is at least one such floor (non-vacuity: a rename that hides them
#      all must fail rather than pass because its subject disappeared).
#
# Deliberately NOT checked here: whether that version is the newest patch of
# its minor. That is a question about the world (it needs the Go release index)
# and it is asked by artifact-keeper/trivy's scripts/check-go-floor.sh for the
# pinned tarball, and answered for the floating FROM by Go itself. This gate is
# purely internal consistency, so it is offline and cannot flake.
#
# Usage: check-go-floor-agreement.sh [<root>]     (default: repo root)
# Exit:  0 consistent, 1 drift, 2 INFRA (could not measure).
set -uo pipefail

ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

if [[ ! -d "$ROOT/docker" ]]; then
  echo "INFRA: no docker/ directory under ${ROOT}" >&2
  exit 2
fi

# `-E` for the alternation in the filename glob; the pattern is anchored to the
# line so a commented-out or prose mention of GO_MIN_PATCH is not a floor.
found="$(grep -REHn --include='Dockerfile*' '^[[:space:]]*ARG[[:space:]]+GO_MIN_PATCH=' "$ROOT/docker" 2>/dev/null)"

if [[ -z "$found" ]]; then
  echo "INFRA: found no 'ARG GO_MIN_PATCH=' in ${ROOT}/docker." >&2
  echo "  Either the floors were renamed or removed -- in which case this gate" >&2
  echo "  no longer measures anything and must be updated, not trusted -- or the" >&2
  echo "  discovery pattern rotted. Both need a human." >&2
  exit 2
fi

# Parse `<path>-<lineno>:ARG GO_MIN_PATCH=<value>` into a table. The value must
# look like X.Y.Z: a floor spelled `1.27` or `latest` would compare equal to
# another such floor while pinning nothing, so it is rejected here rather than
# silently agreed.
declare -a files=() versions=()
first_version=''
drift=0
bad=0

while IFS= read -r line; do
  [[ -n "$line" ]] || continue
  # grep -H output is `<path>:<lineno>:<text>`, so the first colon ends the
  # path and the second ends the lineno. Derive each from `line` directly: a
  # `${loc}` that has already been cut at the first colon has no colon left to
  # cut again, which is how the display once showed the path as the line number.
  file="${line%%:*}"                       # <path>
  rest="${line#*:}"                        # <lineno>:<text>
  lineno="${rest%%:*}"                     # <lineno>
  text="${rest#*:}"                        # <text>
  file="${file#"$ROOT"/}"

  value="${text#*=}"
  value="${value%%[[:space:]]*}"   # tolerate a trailing comment: ARG X=1.2.3 # note
  value="${value%"${value##*[![:space:]]}"}"

  if [[ ! "$value" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "  ✗ ${file}:${lineno}: GO_MIN_PATCH='${value}' is not a X.Y.Z version" >&2
    bad=$((bad + 1))
    continue
  fi

  files+=("${file}:${lineno}")
  versions+=("$value")

  if [[ -z "$first_version" ]]; then
    first_version="$value"
  elif [[ "$value" != "$first_version" ]]; then
    drift=$((drift + 1))
  fi
done <<< "$found"

if [[ $bad -gt 0 ]]; then
  echo
  echo "::error title=Malformed Go toolchain floor::${bad} GO_MIN_PATCH value(s) are not X.Y.Z. A floor that is not a version cannot be compared with the others."
  exit 1
fi

echo "Go toolchain floors under ${ROOT}/docker:"
for i in "${!files[@]}"; do
  echo "  ${files[$i]}  ->  ${versions[$i]}"
done
echo

if [[ $drift -gt 0 ]]; then
  echo "::error title=Go toolchain floors disagree::GO_MIN_PATCH is written with ${#versions[@]} value(s), ${drift} of which differ from ${first_version}. The stdlib is compiled into every binary these floors guard, so a differing floor is an image that did not get a toolchain bump -- which is how #4852 found one affected image and left the others (grype and buildctl in the backend) on the vulnerable stdlib."
  echo
  echo "Fix: raise every GO_MIN_PATCH to the same version in the same change."
  echo "If a deliberate split is ever intended, make it here, with the reason."
  exit 1
fi

echo "::notice::All ${#versions[@]} GO_MIN_PATCH floors agree on ${first_version}."
