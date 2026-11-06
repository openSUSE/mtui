#!/usr/bin/env bash
# Decide whether release tag $1 publishes to crates.io, reading the release
# branch names (`X.Y.x`, one per line) on stdin. Prints, for $GITHUB_OUTPUT:
#
#   publish=true|false
#   branch=<X.Y.x of the tag>
#   reason=<one line>
#
# Only a plain `X.Y.Z` tag with Z >= 1 on the numerically highest `X.Y.x`
# branch publishes: `main` carries `X.Y.0` and never releases, and an older
# line still gets a GitHub release but must not move the crates.io "latest".
# The caller checks separately that the tagged commit is on that branch.
#
# Tested by xtask/tests/crates_io_gate.rs, which runs this file.
set -euo pipefail

tag="${1:?usage: crates-io-gate.sh <tag> < branches}"

emit() {
	printf 'publish=%s\nbranch=%s\nreason=%s\n' "$1" "$2" "$3"
	exit 0
}

tag_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
if [[ ! "$tag" =~ $tag_re ]]; then
	emit false "" "'$tag' is not a plain X.Y.Z release tag"
fi
major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"
branch="$major.$minor.x"

if [ "$patch" -lt 1 ]; then
	emit false "$branch" "$tag is an X.Y.0 development version; only X.Y.Z with Z >= 1 publishes"
fi

branch_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.x$'
have_branch=0
top_major=-1
top_minor=-1
top=""
while IFS= read -r b || [ -n "$b" ]; do
	[[ "$b" =~ $branch_re ]] || continue
	bmaj="${BASH_REMATCH[1]}"
	bmin="${BASH_REMATCH[2]}"
	[ "$b" = "$branch" ] && have_branch=1
	if [ "$bmaj" -gt "$top_major" ] ||
		{ [ "$bmaj" -eq "$top_major" ] && [ "$bmin" -gt "$top_minor" ]; }; then
		top_major="$bmaj"
		top_minor="$bmin"
		top="$b"
	fi
done

if [ "$have_branch" -eq 0 ]; then
	emit false "$branch" "branch $branch does not exist"
fi
if [ "$top" != "$branch" ]; then
	emit false "$branch" "$branch is not the stable branch (highest is $top)"
fi
emit true "$branch" "$tag is on the stable branch $branch"
