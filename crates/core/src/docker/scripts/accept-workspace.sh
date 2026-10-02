set -eu
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  export SPIN_GIT_USERNAME SPIN_GIT_PASSWORD
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0='!f() { printf "username=%s\npassword=%s\n" "$SPIN_GIT_USERNAME" "$SPIN_GIT_PASSWORD"; }; f'
fi
SPIN_BASE_COMMIT="$(git config --get spin.baseCommit || true)"
if [ -z "$SPIN_BASE_COMMIT" ]; then
  SPIN_BASE_COMMIT="$(git reflog show --format=%H HEAD | tail -n 1)"
fi
if [ -z "$SPIN_BASE_COMMIT" ] || ! git cat-file -e "$SPIN_BASE_COMMIT^{commit}"; then
  echo 'Spin cannot determine the immutable Session base commit' >&2
  exit 41
fi
SPIN_HEAD="$(git rev-parse HEAD)"
# A Session that merged the base branch keeps that merge: the result commit
# gets the base as a second parent, so the Job branch knows the base is in
# and the next merge does not meet the same conflict again.
SPIN_MERGED=""
if [ -n "${SPIN_BASE_BRANCH:-}" ] && git rev-parse -q --verify "refs/remotes/origin/${SPIN_BASE_BRANCH}" >/dev/null 2>&1; then
  if git merge-base --is-ancestor "refs/remotes/origin/${SPIN_BASE_BRANCH}" "$SPIN_HEAD" && ! git merge-base --is-ancestor "refs/remotes/origin/${SPIN_BASE_BRANCH}" "$SPIN_BASE_COMMIT"; then
    SPIN_MERGED="$(git rev-parse "refs/remotes/origin/${SPIN_BASE_BRANCH}")"
  fi
fi
SPIN_DIRTY="$(git status --porcelain=v1 --untracked-files=all)"
SPIN_CHANGED=0
if [ "$SPIN_HEAD" != "$SPIN_BASE_COMMIT" ] || [ -n "$SPIN_DIRTY" ]; then
  SPIN_CHANGED=1
fi
SPIN_COMMITTED=0
SPIN_PUBLISH="$SPIN_HEAD"
if [ "$SPIN_CHANGED" = 1 ] && [ "$SPIN_ALLOW_CHANGES" != 1 ]; then
  # A phase without write policy never integrates. The agent may restore,
  # build and experiment in this throwaway workspace; none of it goes along.
  # ACCEPT confirms the untouched base and leaves the workspace as it is.
  SPIN_PUBLISH="$SPIN_BASE_COMMIT"
elif [ "$SPIN_CHANGED" = 1 ]; then
  # Agent-created commits and dirty files are deliberately folded into one
  # control-plane commit so ACCEPT is the only integration boundary.
  git reset --soft "$SPIN_BASE_COMMIT"
  git add -A
  if ! git diff --cached --quiet || [ -n "$SPIN_MERGED" ]; then
    if [ -n "$SPIN_MERGED" ]; then
      SPIN_RESULT="$(printf '%s\n\n%s\n' "$SPIN_COMMIT_SUBJECT" "$SPIN_COMMIT_BODY" | git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit-tree "$(git write-tree)" -p "$SPIN_BASE_COMMIT" -p "$SPIN_MERGED" -F -)"
      git reset -q --hard "$SPIN_RESULT"
    else
      git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit -m "$SPIN_COMMIT_SUBJECT" -m "$SPIN_COMMIT_BODY"
    fi
    SPIN_COMMITTED=1
  else
    git reset --mixed "$SPIN_BASE_COMMIT"
  fi
  SPIN_PUBLISH="$(git rev-parse HEAD)"
fi
if git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_REF" >/dev/null 2>&1; then
  git fetch --depth=50 origin "$SPIN_GIT_REF"
  if ! git merge-base --is-ancestor FETCH_HEAD "$SPIN_PUBLISH"; then
    echo 'The Job branch advanced after this Session started; automatic ACCEPT cannot overwrite it' >&2
    exit 43
  fi
fi
git push origin "$SPIN_PUBLISH:refs/heads/$SPIN_GIT_REF"
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_REF" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_PUBLISH" ]; then
  echo 'Remote Job branch does not match the accepted Session HEAD after push' >&2
  exit 44
fi
# The Session's work-in-progress branch on the remote has served; the Job
# branch carries the result now.
SPIN_SESSION_REF="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [ -n "$SPIN_SESSION_REF" ] && [ "$SPIN_SESSION_REF" != "HEAD" ] && [ "$SPIN_SESSION_REF" != "$SPIN_GIT_REF" ]; then
  git push -q origin ":refs/heads/$SPIN_SESSION_REF" >/dev/null 2>&1 || true
fi
printf 'SPIN_ACCEPT committed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_PUBLISH"
unset SPIN_GIT_PASSWORD