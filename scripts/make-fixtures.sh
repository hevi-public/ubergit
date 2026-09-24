#!/usr/bin/env bash
# Builds a workdir of git repos in every state ubergit must handle.
# Usage: scripts/make-fixtures.sh <dir>   (dir is wiped and recreated)
#
# Each repo gets its own bare "origin" under <dir>/.origins (hidden, so discovery
# skips it) and a helper clone under <dir>/.helpers used to push "upstream" work.
set -euo pipefail

DIR=${1:?usage: make-fixtures.sh <dir>}
rm -rf "$DIR"
mkdir -p "$DIR/.origins" "$DIR/.helpers"
DIR=$(cd "$DIR" && pwd -P)

# Isolate from the user's git config (signing, templates, hooks...).
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=Fixture GIT_AUTHOR_EMAIL=fixture@example.com
export GIT_COMMITTER_NAME=Fixture GIT_COMMITTER_EMAIL=fixture@example.com
export GIT_TERMINAL_PROMPT=0
g() { git -c init.defaultBranch=main -c advice.detachedHead=false -c protocol.file.allow=always "$@"; }

commit() { # commit <repo> <file> <msg>
  echo "$3" >> "$1/$2"
  g -C "$1" add "$2"
  g -C "$1" commit -qm "$3"
}

# origin <name> [branch]: bare origin with two commits on <branch> (default main).
origin() {
  local name=$1 branch=${2:-main}
  g init -q --bare -b "$branch" "$DIR/.origins/$name.git"
  g clone -q "$DIR/.origins/$name.git" "$DIR/.helpers/$name" 2>/dev/null
  g -C "$DIR/.helpers/$name" checkout -q -b "$branch" 2>/dev/null || true
  commit "$DIR/.helpers/$name" README.md "init $name"
  commit "$DIR/.helpers/$name" app.txt "feature one"
  g -C "$DIR/.helpers/$name" push -q origin "$branch"
}

# upstream_work <name> <n> [branch]: push n more commits to origin from the helper.
upstream_work() {
  local name=$1 n=$2 branch=${3:-main}
  g -C "$DIR/.helpers/$name" checkout -q "$branch"
  for i in $(seq "$n"); do commit "$DIR/.helpers/$name" upstream.txt "upstream $i"; done
  g -C "$DIR/.helpers/$name" push -q origin "$branch"
}

clone() { # clone <name> [dest]
  g clone -q "$DIR/.origins/$1.git" "$DIR/${2:-$1}" 2>/dev/null
}

# --- clean & synced
origin synced; clone synced

# --- behind upstream by 2
origin behind; clone behind; upstream_work behind 2; g -C "$DIR/behind" fetch -q

# --- ahead by 1
origin ahead; clone ahead; commit "$DIR/ahead" local.txt "local work"

# --- diverged: ahead 1, behind 2
origin diverged; clone diverged; commit "$DIR/diverged" local.txt "local work"
upstream_work diverged 2; g -C "$DIR/diverged" fetch -q

# --- dirty: 1 staged, 1 unstaged, 1 untracked
origin dirty; clone dirty
echo change >> "$DIR/dirty/README.md"; g -C "$DIR/dirty" add README.md
echo change >> "$DIR/dirty/app.txt"
echo new > "$DIR/dirty/untracked.txt"

# --- feature branch tracking origin/feature; main moved on since (base: ahead 2, behind 1)
origin feature; clone feature
g -C "$DIR/feature" checkout -q -b feature
commit "$DIR/feature" feat.txt "feat a"; commit "$DIR/feature" feat.txt "feat b"
g -C "$DIR/feature" push -q -u origin feature 2>/dev/null
upstream_work feature 1; g -C "$DIR/feature" fetch -q

# --- detached HEAD one commit behind main
origin detached; clone detached; g -C "$DIR/detached" checkout -q HEAD~1

# --- local branch without upstream
origin no-upstream; clone no-upstream
g -C "$DIR/no-upstream" checkout -q -b spike; commit "$DIR/no-upstream" spike.txt "spike"

# --- upstream gone (deleted on remote, pruned locally)
origin gone; clone gone
g -C "$DIR/gone" checkout -q -b old-feature; commit "$DIR/gone" old.txt "old"
g -C "$DIR/gone" push -q -u origin old-feature 2>/dev/null
g -C "$DIR/.helpers/gone" push -q origin --delete old-feature 2>/dev/null
g -C "$DIR/gone" fetch -q --prune

# --- no remote at all
g init -q "$DIR/no-remote"; commit "$DIR/no-remote" a.txt "one"; commit "$DIR/no-remote" a.txt "two"

# --- unborn: no commits yet
g init -q "$DIR/unborn"; echo hi > "$DIR/unborn/new.txt"

# --- stopped in a conflicting rebase
origin rebasing; clone rebasing
echo "local" > "$DIR/rebasing/app.txt"; g -C "$DIR/rebasing" commit -qam "local conflicting"
g -C "$DIR/.helpers/rebasing" checkout -q main
echo "remote" > "$DIR/.helpers/rebasing/app.txt"; g -C "$DIR/.helpers/rebasing" commit -qam "remote conflicting"
g -C "$DIR/.helpers/rebasing" push -q origin main
g -C "$DIR/rebasing" fetch -q
g -C "$DIR/rebasing" rebase -q origin/main >/dev/null 2>&1 || true

# --- merge with a conflict
origin merging; clone merging
g -C "$DIR/merging" checkout -q -b other
echo "other" > "$DIR/merging/app.txt"; g -C "$DIR/merging" commit -qam "other side"
g -C "$DIR/merging" checkout -q main
echo "main" > "$DIR/merging/app.txt"; g -C "$DIR/merging" commit -qam "main side"
g -C "$DIR/merging" merge -q other >/dev/null 2>&1 || true

# --- origin/HEAD missing and default branch is master
origin legacy master; clone legacy; g -C "$DIR/legacy" remote set-head origin -d

# --- two stash entries
origin stashed; clone stashed
echo a >> "$DIR/stashed/app.txt"; g -C "$DIR/stashed" stash -q
echo b >> "$DIR/stashed/app.txt"; g -C "$DIR/stashed" stash -q

# --- main checkout plus a linked worktree next to it
origin wt; clone wt wt-main
g -C "$DIR/wt-main" worktree add -q -b wt-branch "$DIR/wt-linked" 2>/dev/null

# --- repo containing a submodule (the submodule must not be listed separately)
origin lib; origin with-submodule; clone with-submodule
g -C "$DIR/with-submodule" submodule -q add "$DIR/.origins/lib.git" libs/lib 2>/dev/null
g -C "$DIR/with-submodule" commit -qm "add submodule"

# --- shallow clone
origin shallow; upstream_work shallow 3
g clone -q --depth 1 "file://$DIR/.origins/shallow.git" "$DIR/shallow" 2>/dev/null

# --- bare repo inside the workdir
g init -q --bare "$DIR/bare-repo.git"

# --- nested one level down, and one hidden inside node_modules (must be skipped)
origin nested; clone nested group/nested-svc
g init -q "$DIR/group/node_modules/ignored"

echo "$DIR"
