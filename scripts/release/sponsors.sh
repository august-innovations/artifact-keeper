#!/usr/bin/env bash
# =============================================================================
# scripts/release/sponsors.sh -- render the recognition block for sponsors
# =============================================================================
# The `### Sponsors` section of every CHANGELOG release section and the
# `## Sponsors` section of every `.github/release-notes/<version>.md` used to
# be copied by hand from README.md, which was itself copied by hand from the
# GitHub Sponsors dashboard, so the list drifted: 1.11.1 shipped naming two
# backers while seven sponsorships were active. This script asks GitHub for
# the ACTIVE sponsorships of the artifact-keeper organization and prints the
# block, so the release prep PR pastes the output instead of a memory.
#
#   scripts/release/sponsors.sh            # `### Sponsors` block for CHANGELOG.md
#   scripts/release/sponsors.sh --level 2  # `## Sponsors` block for release notes
#   scripts/release/sponsors.sh --json     # the raw list, for other tooling
#   scripts/release/sponsors.sh --check FILE
#                                          # exit 1 if FILE's Sponsors section
#                                          # does not name every public sponsor
#
# Privacy. A sponsor can choose a PRIVATE sponsorship; GitHub shows them to
# the maintainer but to nobody else. Private sponsors are never named here:
# they are COUNTED, and the block ends with "and N sponsors who chose to stay
# private". The two queries below differ only in `includePrivate`; the public
# query's result is what gets printed, and the difference in `totalCount` is
# the private count. (The `privacyLevel` field would say the same per row,
# but it needs the `read:user` token scope, which the default `gh auth` token
# lacks; the two-query form works with `read:org` alone.)
#
# Ordering: by monthly amount descending, then by login, so the block is
# stable between runs and the people giving the most are listed first; the
# amounts themselves are NOT printed.
#
# Requirements: `gh` authenticated as an admin of the organization (the
# sponsorship list is maintainer-only) and `jq`.
# =============================================================================
set -euo pipefail

ORG="${SPONSORS_ORG:-artifact-keeper}"
LEVEL=3
MODE=render
CHECK_FILE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --level) LEVEL="$2"; shift 2 ;;
    --org) ORG="$2"; shift 2 ;;
    --json) MODE=json; shift ;;
    --check) MODE=check; CHECK_FILE="$2"; shift 2 ;;
    -h|--help) sed -n '2,37p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "sponsors.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$LEVEL" in 2|3) ;; *) echo "sponsors.sh: --level must be 2 or 3" >&2; exit 2 ;; esac

for cmd in gh jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "sponsors.sh: $cmd is required" >&2; exit 2; }
done

query() { # includePrivate(true|false) -> JSON of sponsorshipsAsMaintainer
  gh api graphql -f query="query {
    organization(login: \"${ORG}\") {
      sponsorshipsAsMaintainer(first: 100, includePrivate: $1, activeOnly: true,
                               orderBy: {field: CREATED_AT, direction: ASC}) {
        totalCount
        nodes {
          tier { monthlyPriceInDollars isOneTime }
          sponsorEntity {
            __typename
            ... on User { login name }
            ... on Organization { login name }
          }
        }
      }
    }
  }" --jq '.data.organization.sponsorshipsAsMaintainer'
}

public_json="$(query false)"
all_json="$(query true)"
if [ -z "$public_json" ] || [ "$public_json" = "null" ]; then
  echo "sponsors.sh: GitHub returned no sponsorship data for ${ORG}; is gh logged in as an org admin?" >&2
  exit 1
fi

public_count="$(jq -r '.totalCount' <<<"$public_json")"
all_count="$(jq -r '.totalCount' <<<"$all_json")"
private_count=$(( all_count - public_count ))
if [ "$public_count" -ge 100 ]; then
  echo "sponsors.sh: more than 100 public sponsorships; add pagination before trusting this list" >&2
  exit 1
fi

# One row per public sponsor: login, display name (falls back to the login),
# monthly amount for ordering.
rows="$(jq -c '[.nodes[] | {
    login: .sponsorEntity.login,
    name: (.sponsorEntity.name // .sponsorEntity.login),
    monthly: (.tier.monthlyPriceInDollars // 0)
  }] | sort_by(-.monthly, .login)' <<<"$public_json")"

if [ "$MODE" = json ]; then
  jq -n --argjson rows "$rows" --argjson private "$private_count" \
    '{org: $ENV.ORG, public: $rows, private_count: $private}' ORG="$ORG"
  exit 0
fi

if [ "$MODE" = check ]; then
  [ -f "$CHECK_FILE" ] || { echo "sponsors.sh: no such file: ${CHECK_FILE}" >&2; exit 2; }
  missing=0
  while IFS= read -r login; do
    if ! grep -Eq "github\.com/${login}([)\"/]|$)" "$CHECK_FILE"; then
      echo "sponsors.sh: ${CHECK_FILE} does not name public sponsor @${login}" >&2
      missing=1
    fi
  done < <(jq -r '.[].login' <<<"$rows")
  if [ "$private_count" -gt 0 ] && ! grep -qi "stay private\|remain private\|private sponsor" "$CHECK_FILE"; then
    echo "sponsors.sh: ${CHECK_FILE} does not acknowledge the ${private_count} private sponsorship(s)" >&2
    missing=1
  fi
  exit "$missing"
fi

hashes="$(printf '#%.0s' $(seq 1 "$LEVEL"))"
echo "${hashes} Sponsors"
echo
if [ "$public_count" -eq 0 ] && [ "$private_count" -eq 0 ]; then
  echo "[Become a sponsor](https://github.com/sponsors/${ORG}) to support the project and get your name listed here."
  exit 0
fi
echo "Thank you to our sponsors for supporting ongoing development of Artifact Keeper."
echo
echo "**Backers**"
echo
jq -r '.[] | "- **\(.name)** ([@\(.login)](https://github.com/\(.login)))"' <<<"$rows"
if [ "$private_count" -eq 1 ]; then
  echo "- and one sponsor who chose to stay private"
elif [ "$private_count" -gt 1 ]; then
  echo "- and ${private_count} sponsors who chose to stay private"
fi
echo
echo "[Become a sponsor](https://github.com/sponsors/${ORG}) to support the project and get your name listed here."
