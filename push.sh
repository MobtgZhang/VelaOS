#!/usr/bin/env bash
#
# VelaOS — the push helper.
#
# # What this script does
#
# It checks three things and then tells you how to push:
#
#   1. the four feature commits are in the history, in order;
#   2. the working tree is clean;
#   3. how many commits are ahead of `origin/main`.
#
# It never rewrites history. An earlier version of this script split the
# namespace, shell and desktop work into four per-feature commits, and it
# could undo that split and redo it. That split is done and committed, and
# the commits that came after it — the Exec objects, the architecture layers
# and the FAT volume — are committed too. Unwinding anything now would
# destroy work that is already in place, so the script verifies and stops.
#
# # Usage
#
#   ./push.sh          check the history, print the log, print the command
#   PUSH=1 ./push.sh   do the same, and then push
#
# The push is opt-in because it is the one step that talks to the outside
# world: run it when you have read the log and agree with it.

set -euo pipefail

cd "$(dirname "$0")"

# The four commits the split produced, oldest first. They are checked by
# subject, in order, anywhere in the history: the commits that followed them
# are as much part of the work as they are.
EXPECTED=(
  "dos.library: AddAssign, RemoveAssign and ListAssigns across the boundary"
  "shell: a real AmigaDOS interpreter"
  "init: grow SYS: to the full AROS system-volume shape"
  "workbench: spell the volume icons the way AmigaDOS does"
)

# The commit the split started from.
BASE="99d6c59"

say() { printf '%s\n' "$*"; }
fail() {
  say "$1" >&2
  exit 1
}

# The subjects of the commits since the base, oldest first.
subjects_since_base() {
  git log --reverse --pretty=%s "${BASE}..HEAD" 2>/dev/null || true
}

# Are the four feature commits in the history, in order?
chain_in_place() {
  local subjects
  subjects="$(subjects_since_base)"
  local previous=0
  local want
  local line
  for want in "${EXPECTED[@]}"; do
    # The first line that carries this subject exactly.
    line="$(printf '%s\n' "$subjects" | grep -n -F -x "$want" | head -1 | cut -d: -f1)"
    [ -n "$line" ] || return 1
    # And it must come after the one before it: the order is the point.
    [ "$line" -gt "$previous" ] || return 1
    previous="$line"
  done
  return 0
}

say "VelaOS — checking the history before you push."
say ""

if ! chain_in_place; then
  fail "The four feature commits are not all in the history since ${BASE}.
Inspect 'git log --oneline' before doing anything else."
fi
say "The four feature commits are in place."

# Only *tracked* files matter here. The reports under `reports/` are the
# project's own analysis documents and are deliberately not committed, so
# they must not stop the push.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  say ""
  say "The working tree is not clean:"
  git status --short --untracked-files=no | sed 's/^/  /'
  fail "Commit or stash the changes above before pushing."
fi
say "The working tree is clean."

ahead="$(git rev-list --count origin/main..HEAD 2>/dev/null || echo 0)"
say "${ahead} commit(s) ahead of origin/main."
say ""
say "Commits:"
git log --oneline "${BASE}..HEAD" | sed 's/^/  /'
say ""

if [ "${PUSH:-0}" = "1" ]; then
  say "Pushing to origin/main."
  git push origin main
  say "Pushed."
else
  say "Review the log above, then push when satisfied:"
  say "  git push origin main"
  say "or run 'PUSH=1 ./push.sh' to do both in one step."
fi
