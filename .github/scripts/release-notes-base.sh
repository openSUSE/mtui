#!/usr/bin/env bash
# Print the release tag immediately preceding $1 in SemVer precedence order,
# reading the candidate tags on stdin (one per line). Empty output means $1 is
# the oldest, i.e. the first release.
#
# Not `sort -V`: that orders `26.4.0` *before* `26.4.0-rc1`, so a patch release
# would take a prerelease as its notes base and re-list what the prior release
# already shipped. SemVer §11 says a prerelease has *lower* precedence than the
# release it precedes. Build metadata (`+...`) is ignored for precedence (§10).
#
# Tested by xtask/tests/release_notes_base.rs, which runs this file.
set -euo pipefail

current="${1:?usage: release-notes-base.sh <current-tag> < tags}"

# `<major>.<minor>.<patch>` with an optional `-<prerelease>` and `+<build>`,
# each identifier alphanumeric-or-hyphen and numeric ones without leading zeros.
semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'

# A sort key whose plain byte order (LC_ALL=C) is SemVer precedence order.
# Numeric fields are zero-padded so they compare numerically; the release/
# prerelease marker is `1`/`0` so a prerelease sorts before its release; within
# a prerelease each identifier is tagged `0` numeric / `1` alphanumeric, since
# numeric identifiers always have lower precedence.
semver_key() {
    local v="$1" core pre ma mi pa key id
    v="${v%%+*}"
    core="${v%%-*}"
    if [ "$v" = "$core" ]; then pre=""; else pre="${v#*-}"; fi
    IFS=. read -r ma mi pa <<<"$core"
    key="$(printf '%010d.%010d.%010d.' "$ma" "$mi" "$pa")"
    if [ -z "$pre" ]; then
        printf '%s1' "$key"
        return
    fi
    key="${key}0"
    while IFS= read -r id || [ -n "$id" ]; do
        if [[ "$id" =~ ^[0-9]+$ ]]; then
            key+="$(printf '.0%010d' "$id")"
        else
            key+=".1$id"
        fi
    done < <(printf '%s\n' "$pre" | tr '.' '\n')
    printf '%s' "$key"
}

keyed=""
seen_current=0
while IFS= read -r tag || [ -n "$tag" ]; do
    [ -n "$tag" ] || continue
    if [[ ! "$tag" =~ $semver_re ]]; then
        echo "::error::not a SemVer tag: '$tag'" >&2
        exit 1
    fi
    [ "$tag" = "$current" ] && seen_current=1
    keyed+="$(semver_key "$tag")	$tag"$'\n'
done

if [ "$seen_current" -eq 0 ]; then
    echo "::error::release tag '$current' is absent from the ordered tag set" >&2
    exit 1
fi

# Walk the ordered tags and stop at `current`; `prev` is then its predecessor,
# and stays empty when `current` is the oldest. An explicit walk rather than
# `grep -B1`, which exits 1 on no match and would fail the release.
prev=""
while IFS= read -r tag || [ -n "$tag" ]; do
    [ "$tag" = "$current" ] && break
    prev="$tag"
done < <(printf '%s' "$keyed" | LC_ALL=C sort | cut -f2-)

printf '%s\n' "$prev"
