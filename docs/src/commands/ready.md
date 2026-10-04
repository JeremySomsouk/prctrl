# ready

Explain each PR's readiness using the same read-only engine as the TUI.

```bash
prctrl ready
prctrl ready 123
prctrl ready --pr-numbers 123,456 --json
prctrl ready --all --repo frontend --author alice --since-days 7 --priority
```

`--pr` (including the global flag) takes precedence over positional numbers and
`--pr-numbers`. Explicit targets bypass the pending-review prefetch. By default,
this command analyzes pending reviews; `--all` is an explicit synonym.
Repository and author filters are case-insensitive partial matches.

## Evidence and states

- **READY**: open, not draft, mergeable, GitHub merge state CLEAN, review policy
  satisfied, and complete passing CI evidence for the head commit.
- **BLOCKED**: a definite blocker, such as pending/failing CI, outstanding reviews,
  changes requested, conflicts, a draft, or a branch behind its base.
- **UNKNOWN**: missing permissions, network failure, unsupported/unknown states,
  no CI evidence, inconsistent head evidence, or incomplete/truncated contexts.

The report includes individual checks, reasons, the full head SHA and the UTC
observation time. GitHub Check Runs and legacy status contexts are both included.
Completed SUCCESS, NEUTRAL and SKIPPED checks pass; pending checks never do.
A null review decision with a CLEAN merge state means `not_required`, not an approval.
Missing review policy never becomes an approval. Definite blockers take precedence
when uncertainty is also present; both remain visible in the reasons.

Requests share a connection pool, with at most four concurrent readiness requests,
30-second timeouts and rate-limit cooldowns. Strict transports do not hide HTTP
rate limits behind automatic retries. At most 100 check contexts are fetched in
one snapshot; a larger/incomplete set produces UNKNOWN unless a definite blocker
is already known. Explicit target lookups also use four workers and stop on errors.

This is a conservative snapshot, not merge authorization. GitHub enforces current
rules, permissions and merge-queue requirements at merge time.

## JSON and compatibility

`--json` still returns an array with the existing PR metadata, `approved`,
`ci_status`, `has_conflicts`, `draft`, and optional `priority_score`. `approved`
now reflects GitHub's actual APPROVED review decision. `draft` is null if evidence
could not be obtained. Added fields are `state` (`ready`, `blocked`, `unknown`),
`head_sha`, `observed_at`, `review_decision`, `merge_state`, `blockers`, and `checks`.
Scripts should use `state == "ready"`; pending CI no longer counts as ready.
Results sort READY, BLOCKED, UNKNOWN, then increasing age, repository and PR number.

```bash
prctrl ready --json | jq '.[] | select(.state == "ready")'
```
