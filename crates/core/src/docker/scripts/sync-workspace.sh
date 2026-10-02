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
SPIN_COMMITTED=0
git add -A
if ! git diff --cached --quiet; then
  git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit -q -m "WIP $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  SPIN_COMMITTED=1
fi
SPIN_HEAD="$(git rev-parse HEAD)"
SPIN_PUSHED=0
SPIN_REMOTE_HEAD="$(git ls-remote origin "refs/heads/${SPIN_SESSION_REF}" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_HEAD" ]; then
  git push -q -f origin "$SPIN_HEAD:refs/heads/${SPIN_SESSION_REF}"
  SPIN_PUSHED=1
fi
printf 'SPIN_SYNC committed=%s pushed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_PUSHED" "$SPIN_HEAD"
unset SPIN_GIT_PASSWORD