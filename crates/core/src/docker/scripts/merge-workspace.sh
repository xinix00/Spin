export SPIN_GIT_USERNAME SPIN_GIT_PASSWORD
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0='!f() { printf "username=%s\npassword=%s\n" "$SPIN_GIT_USERNAME" "$SPIN_GIT_PASSWORD"; }; f'
set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  export GIT_CONFIG_COUNT=1
fi
git fetch -q --depth=200 origin "+refs/heads/${SPIN_MERGE_TARGET}:refs/remotes/origin/${SPIN_MERGE_TARGET}"
git fetch -q --depth=200 origin "+refs/heads/${SPIN_MERGE_SOURCE}:refs/remotes/origin/${SPIN_MERGE_SOURCE}"
SPIN_SOURCE="$(git rev-parse "refs/remotes/origin/${SPIN_MERGE_SOURCE}")"
SPIN_TARGET="$(git rev-parse "refs/remotes/origin/${SPIN_MERGE_TARGET}")"
git checkout -q -B spin-merge "$SPIN_TARGET"
# Always a merge commit, never a fast-forward: the base branch then shows
# one commit per Job, with the Job's own commits visible underneath it.
if ! git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" merge -q --no-ff -m "$SPIN_COMMIT_SUBJECT" -m "$SPIN_COMMIT_BODY" "$SPIN_SOURCE" >/dev/null 2>&1; then
  SPIN_CONFLICTS="$(git diff --name-only --diff-filter=U | tr '\n' ' ' | sed 's/ $//')"
  git merge --abort >/dev/null 2>&1 || true
  echo "SPIN_CONFLICT De Job-branch conflicteert met ${SPIN_MERGE_TARGET} in: ${SPIN_CONFLICTS}. Merge origin/${SPIN_MERGE_TARGET} in de Job-branch (die staat al opgehaald, niet fetchen), los de conflicten op en accept; Spin neemt het resultaat op in de Job-branch en de merge in ${SPIN_MERGE_TARGET} kan daarna opnieuw." >&2
  exit 45
fi
SPIN_HEAD="$(git rev-parse HEAD)"
git push origin "$SPIN_HEAD:refs/heads/${SPIN_MERGE_TARGET}"
# ls-remote matches a bare name against the tail of every ref: "main" would
# also list jobs/<ticket>/main. The full ref name is exact.
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code origin "refs/heads/${SPIN_MERGE_TARGET}" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_HEAD" ]; then
  echo "Remote ${SPIN_MERGE_TARGET} does not match the merged HEAD after push" >&2
  exit 46
fi
printf 'SPIN_MERGE head=%s\n' "$SPIN_HEAD"
unset SPIN_GIT_PASSWORD