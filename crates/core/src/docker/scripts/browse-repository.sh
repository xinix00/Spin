set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
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
case "$SPIN_MODE" in
  refs)
    # Every branch tip, commits and trees only (no blobs), so the branches
    # can be ordered by their last commit; blobs come lazily when a file is
    # read. An old git without partial clone fetches the tips whole.
    git fetch -q --prune --depth=1 --filter=blob:none origin '+refs/heads/*:refs/remotes/origin/*' 2>/dev/null \
      || git fetch -q --prune --depth=1 origin '+refs/heads/*:refs/remotes/origin/*'
    git for-each-ref --sort=-committerdate --count=300 --format='%(committerdate:unix)%09%(refname:strip=3)' refs/remotes/origin
    ;;
  tree)
    git fetch -q --depth=1 origin "+refs/heads/${SPIN_REF}:refs/remotes/origin/${SPIN_REF}" 2>/dev/null
    git ls-tree -r -l "refs/remotes/origin/${SPIN_REF}" | while IFS= read -r line; do
      meta="${line%%	*}"; path="${line#*	}"; size="${meta##* }"
      printf '%s\t%s\n' "$size" "$path"
    done
    ;;
  file)
    git fetch -q --depth=1 origin "+refs/heads/${SPIN_REF}:refs/remotes/origin/${SPIN_REF}" 2>/dev/null
    printf 'SPIN_SIZE %s\n' "$(git cat-file -s "refs/remotes/origin/${SPIN_REF}:${SPIN_PATH}")"
    git show "refs/remotes/origin/${SPIN_REF}:${SPIN_PATH}" | head -c "$SPIN_LIMIT"
    ;;
esac
unset SPIN_GIT_PASSWORD