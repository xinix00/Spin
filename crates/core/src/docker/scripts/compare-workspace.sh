set -eu
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  export SPIN_GIT_USERNAME SPIN_GIT_PASSWORD
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0='!f() { printf "username=%s\npassword=%s\n" "$SPIN_GIT_USERNAME" "$SPIN_GIT_PASSWORD"; }; f'
fi
git fetch -q --depth=256 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
# A merged Job: the base branch holds the Job now, so the merge itself is
# the comparison: its first parent against the merge commit.
if [ -n "$SPIN_COMPARE_MERGE" ]; then
  if ! git cat-file -e "$SPIN_COMPARE_MERGE^{commit}" 2>/dev/null; then
    git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base"
  fi
  SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE^1")"
  SPIN_COMPARE_HEAD_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE")"
  printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
  unset SPIN_GIT_PASSWORD
  exit 0
fi
SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base HEAD || true)"
if [ -z "$SPIN_COMPARE_BASE_COMMIT" ]; then
  git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
  SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base HEAD || true)"
fi
test -n "$SPIN_COMPARE_BASE_COMMIT"
SPIN_COMPARE_HEAD_COMMIT=""
if [ -n "$SPIN_COMPARE_COMMIT_MATCH" ]; then
  SPIN_COMPARE_HEAD_COMMIT="$(git log refs/remotes/spin/head --fixed-strings --grep="$SPIN_COMPARE_COMMIT_MATCH" -1 --format=%H)"
  if [ -n "$SPIN_COMPARE_HEAD_COMMIT" ]; then
    SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_HEAD_COMMIT^")"
  fi
fi
printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
unset SPIN_GIT_PASSWORD