# Repository grade status and coverage

Since 1.12.0 (#4336) a repository security score says how much of the
repository was assessed, instead of turning the grade letter into an F when
something could not be assessed.

## Fields

`GET /api/v1/security/scores` and `GET /api/v1/repositories/{key}/security`
(`score`) return:

| Field | Meaning |
|---|---|
| `score` | 0 to 100, from the open findings of the latest completed scans (unchanged). |
| `grade` | The letter for `score` over the assessed artifacts (A to F), `null` when `grade_status` is `unrated`. Never an F floor. |
| `grade_status` | `rated`, `incomplete` or `unrated`, below. |
| `coverage` | Counts per live artifact, below. `null` until a 1.12 replica has recalculated the score. |
| `has_failed_scan`, `has_uncataloged_scan` | Unchanged flags (#2167, #4036). |

`coverage` counts each live (not deleted) artifact once, by the latest scan
of each scanner:

| Field | Meaning |
|---|---|
| `artifacts` | Live artifacts in the repository. |
| `scanned` | Artifacts with a finished scan: `cataloged + failed + uncataloged`. |
| `failed` | Some latest scan failed. |
| `uncataloged` | Not failed, and some latest completed scan cataloged nothing (`scan_completeness = not_cataloged`). |
| `cataloged` | Assessed: the rest of `scanned`. |
| `partial` | The part of `cataloged` with a `partial` latest scan. |
| `coverage_pct` | `(cataloged - partial) / scanned`, whole percent rounded down; `null` when nothing was scanned. |

Artifacts with no finished scan (none yet, pending, running, or every scanner
not applicable) are in `artifacts` but not in `scanned`.

## Status

| `grade_status` | When |
|---|---|
| `rated` | At least one artifact assessed, and every scanned artifact fully assessed (`cataloged > 0`, `failed = uncataloged = partial = 0`). |
| `incomplete` | Something assessed (`cataloged > 0`) and something not, or only in part (`failed`, `uncataloged` or `partial` above zero). The letter covers the assessed artifacts. Also every repository with findings, or with an artifact some scanner cataloged, that the counts alone would call `unrated`: an artifact one scanner cataloged and another failed on counts as `failed`, and the findings of an earlier completed scan still count after a failed rescan. Findings always come with their letter. |
| `unrated` | Nothing assessed (`cataloged = 0`, no artifact cataloged by any scanner and no findings): no scans at all, or every scan failed or cataloged nothing. No letter. |

For a `rated` repository the letter is exactly the one earlier releases
showed. Deleting an artifact recalculates its repository's score, so removing
the artifact that could not be assessed makes the repository `rated` without
waiting for the next scan.

The security dashboard (`GET /api/v1/security/dashboard`) counts
repositories by status (`repos_rated`, `repos_incomplete`, `repos_unrated`);
`repos_grade_a` and `repos_grade_f` count the letters shown, so an `unrated`
repository is in neither.

## Per scan

Each scan row (`GET /api/v1/security/scans`, the repository and artifact
scan listings) carries `grade_status` and `grade_status_reason` in the same
vocabulary:

| Scan row | `grade_status` | `grade_status_reason` |
|---|---|---|
| completed, `scan_completeness = complete` | `rated` | `null` |
| completed, `partial` | `incomplete` | `scan_completeness_reason` |
| completed, `not_cataloged` | `unrated` | `scan_completeness_reason` |
| failed | `unrated` | points at `error_message` |
| not applicable, pending, running | `unrated` | why |

A scanner that can read only part of an artifact (for example an image whose
root filesystem could be extracted only in part) records `partial` with a
`scan_completeness_reason`, which reads as `incomplete` with that reason; one
that can read nothing records `not_cataloged`. `inventory_status` (whether the
SBOM rows were stored) does not affect the status.

## Rolling upgrade

Migration 303 adds nullable columns only. 1.12 keeps writing the `grade`
column exactly as 1.11 does, F floor included, so a 1.11 replica reads the
same letter it always did. A score a 1.11 replica writes during the upgrade
leaves the new columns empty or stale (detected by `status_calculated_at`);
1.12 then derives the status from the stored flags (`incomplete` when the
letter was floored or a flag is set, else `rated`) and reports
`coverage: null` until its own next recalculation.

The gates are unchanged: quality gates, promotion rules and download
policies read the numeric `score`, as before.
