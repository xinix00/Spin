set -eu
command -v git >/dev/null
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
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$SPIN_GIT_REMOTE"
else
  git remote set-url origin "$SPIN_GIT_REMOTE"
fi
SPIN_SESSION_HEAD=""
if git ls-remote --exit-code origin "refs/heads/${SPIN_SESSION_REF}" >/dev/null 2>&1; then
  git fetch -q --depth=50 origin "+refs/heads/${SPIN_SESSION_REF}:refs/remotes/origin/${SPIN_SESSION_REF}"
  SPIN_SESSION_HEAD="$(git rev-parse "refs/remotes/origin/${SPIN_SESSION_REF}")"
fi
SPIN_JOB_HEAD=""
if git ls-remote --exit-code origin "refs/heads/${SPIN_GIT_REF}" >/dev/null 2>&1; then
  git fetch -q --depth=50 origin "+refs/heads/${SPIN_GIT_REF}:refs/remotes/origin/${SPIN_GIT_REF}"
  SPIN_JOB_HEAD="$(git rev-parse "refs/remotes/origin/${SPIN_GIT_REF}")"
  if [ -n "$SPIN_BOOTSTRAP_REF" ]; then
    git fetch -q --depth=50 origin "+refs/heads/${SPIN_BOOTSTRAP_REF}:refs/remotes/origin/${SPIN_BOOTSTRAP_REF}" 2>/dev/null || true
  fi
elif [ -n "$SPIN_BOOTSTRAP_REF" ]; then
  git fetch -q --depth=50 origin "+refs/heads/${SPIN_BOOTSTRAP_REF}:refs/remotes/origin/${SPIN_BOOTSTRAP_REF}"
  SPIN_JOB_HEAD="$(git rev-parse "refs/remotes/origin/${SPIN_BOOTSTRAP_REF}")"
fi
if [ -z "$SPIN_JOB_HEAD" ]; then
  echo 'Spin cannot find the Job branch to accept onto' >&2
  exit 41
fi
# A Session that never pushed a branch wrote nothing: the Job branch as it
# stands is the result.
if [ -n "$SPIN_SESSION_HEAD" ] && ! git merge-base --is-ancestor "$SPIN_JOB_HEAD" "$SPIN_SESSION_HEAD"; then
  echo 'The Job branch advanced after this Session started; automatic ACCEPT cannot overwrite it' >&2
  exit 43
fi
SPIN_COMMITTED=0
SPIN_PUBLISH="$SPIN_JOB_HEAD"
if [ "$SPIN_ALLOW_CHANGES" = 1 ] && [ -n "$SPIN_SESSION_HEAD" ] && [ "$SPIN_SESSION_HEAD" != "$SPIN_JOB_HEAD" ]; then
  SPIN_TREE="$(git rev-parse "${SPIN_SESSION_HEAD}^{tree}")"
  # A Session that merged the base branch keeps that merge as a second
  # parent, so the Job branch records that the base is in.
  SPIN_MERGED=""
  if [ -n "$SPIN_BOOTSTRAP_REF" ] && git rev-parse -q --verify "refs/remotes/origin/${SPIN_BOOTSTRAP_REF}" >/dev/null 2>&1; then
    if git merge-base --is-ancestor "refs/remotes/origin/${SPIN_BOOTSTRAP_REF}" "$SPIN_SESSION_HEAD" && ! git merge-base --is-ancestor "refs/remotes/origin/${SPIN_BOOTSTRAP_REF}" "$SPIN_JOB_HEAD"; then
      SPIN_MERGED="$(git rev-parse "refs/remotes/origin/${SPIN_BOOTSTRAP_REF}")"
    fi
  fi
  if [ "$SPIN_TREE" != "$(git rev-parse "${SPIN_JOB_HEAD}^{tree}")" ] || [ -n "$SPIN_MERGED" ]; then
    if [ -n "$SPIN_MERGED" ]; then
      SPIN_PUBLISH="$(printf '%s\n\n%s\n' "$SPIN_COMMIT_SUBJECT" "$SPIN_COMMIT_BODY" | git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit-tree "$SPIN_TREE" -p "$SPIN_JOB_HEAD" -p "$SPIN_MERGED" -F -)"
    else
      SPIN_PUBLISH="$(printf '%s\n\n%s\n' "$SPIN_COMMIT_SUBJECT" "$SPIN_COMMIT_BODY" | git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit-tree "$SPIN_TREE" -p "$SPIN_JOB_HEAD" -F -)"
    fi
    SPIN_COMMITTED=1
  fi
fi
if [ "$SPIN_PUBLISH" != "$SPIN_JOB_HEAD" ] || ! git ls-remote --exit-code origin "refs/heads/${SPIN_GIT_REF}" >/dev/null 2>&1; then
  git push -q origin "$SPIN_PUBLISH:refs/heads/${SPIN_GIT_REF}"
fi
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code origin "refs/heads/${SPIN_GIT_REF}" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_PUBLISH" ]; then
  echo 'Remote Job branch does not match the accepted Session HEAD after push' >&2
  exit 44
fi
if [ -n "$SPIN_SESSION_HEAD" ]; then
  git push -q origin ":refs/heads/${SPIN_SESSION_REF}" >/dev/null 2>&1 || true
fi
printf 'SPIN_ACCEPT committed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_PUBLISH"
unset SPIN_GIT_PASSWORD