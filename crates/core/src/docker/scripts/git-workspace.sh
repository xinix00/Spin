set -eu
command -v git >/dev/null
mkdir -p "${SPIN_GIT_DIR:-.}" && cd "${SPIN_GIT_DIR:-.}"
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
  test "$(git config --get remote.origin.url)" = "$SPIN_GIT_REMOTE"
fi
if ! git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_TARGET" >/dev/null 2>&1; then
  git fetch --depth=1 origin "$SPIN_GIT_BOOTSTRAP"
  SPIN_BOOTSTRAP_HEAD="$(git rev-parse FETCH_HEAD)"
  if ! git push origin "$SPIN_BOOTSTRAP_HEAD:refs/heads/$SPIN_GIT_TARGET"; then
    git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_TARGET" >/dev/null
  fi
fi
# A workspace whose Session branch exists is complete: reuse it as it is.
# Shallow fetches must not be repeated into an existing shallow clone (git
# refuses with "shallow file has changed"), so a half-made workspace, from
# a launch that failed before the branch existed, starts over instead.
if git show-ref --verify --quiet "refs/heads/$SPIN_GIT_HEAD"; then
  git checkout -q "$SPIN_GIT_HEAD"
else
  if [ -e .git/shallow ] || [ -n "$(git for-each-ref refs/remotes 2>/dev/null)" ]; then
    # Nothing of value exists yet: the Session branch was never made.
    find . -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    git init -q
    git remote add origin "$SPIN_GIT_REMOTE"
  fi
  # The agent must be able to see what the Job did before this phase and
  # where the Job will land: the base branch as a remote ref, and the Job
  # branch with its history since that base (falling back to a bounded
  # depth when the remote cannot exclude by ref). Both are shallow; nothing
  # older than the Job is pulled in.
  # The base branch comes with history, not just its tip: a step that has
  # to merge it needs a common ancestor with the Job branch, and a shallow
  # tip has none.
  git fetch -q --depth=200 origin "+refs/heads/${SPIN_GIT_BOOTSTRAP}:refs/remotes/origin/${SPIN_GIT_BOOTSTRAP}" || true
  git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}" 2>/dev/null \
    || git fetch -q --depth=100 origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}"
  # Branches given as context (the Job this one continues) come along as
  # remote refs, with their commits since the base, read-only.
  for SPIN_CONTEXT_REF in ${SPIN_GIT_CONTEXT:-}; do
    git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_CONTEXT_REF}:refs/remotes/origin/${SPIN_CONTEXT_REF}" 2>/dev/null \
      || git fetch -q --depth=100 origin "+refs/heads/${SPIN_CONTEXT_REF}:refs/remotes/origin/${SPIN_CONTEXT_REF}" || true
  done
  # A Session whose work in progress was pushed continues from that branch
  # (on any runner); its base stays the Job branch it started from.
  if git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_HEAD" >/dev/null 2>&1; then
    git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_GIT_HEAD}:refs/remotes/origin/${SPIN_GIT_HEAD}" 2>/dev/null \
      || git fetch -q --depth=100 origin "+refs/heads/${SPIN_GIT_HEAD}:refs/remotes/origin/${SPIN_GIT_HEAD}"
    git checkout -q -B "$SPIN_GIT_HEAD" "refs/remotes/origin/${SPIN_GIT_HEAD}"
    git config spin.baseCommit "$(git merge-base "refs/remotes/origin/${SPIN_GIT_BASE}" HEAD 2>/dev/null || git rev-parse "refs/remotes/origin/${SPIN_GIT_BASE}")"
  else
    git checkout -q -B "$SPIN_GIT_HEAD" "refs/remotes/origin/${SPIN_GIT_BASE}"
  fi
fi
# Merging needs the commit both branches come from, not just their two tips:
# a shallow fetch cuts exactly that commit away, and git then refuses with
# "unrelated histories". The workspace stays shallow; both branches are
# deepened here until they share that commit, because the agent has no
# credentials and cannot fetch anything itself.
if [ -n "$SPIN_GIT_BOOTSTRAP" ] && git rev-parse -q --verify "refs/remotes/origin/${SPIN_GIT_BOOTSTRAP}" >/dev/null 2>&1; then
  SPIN_DEEPEN=0
  while [ "$SPIN_DEEPEN" -lt 3 ] && ! git merge-base "refs/remotes/origin/${SPIN_GIT_BOOTSTRAP}" HEAD >/dev/null 2>&1; do
    # Named refspecs: "git fetch --deepen origin <branch>" leaves the
    # remote-tracking ref where it was, so the merge base never appears.
    git fetch -q --deepen=200 origin \
      "+refs/heads/${SPIN_GIT_BOOTSTRAP}:refs/remotes/origin/${SPIN_GIT_BOOTSTRAP}" \
      "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}" 2>/dev/null || break
    SPIN_DEEPEN=$((SPIN_DEEPEN+1))
  done
fi
# A step that has to resolve a merge finds it already started: Spin merges
# here, where the credentials are, and leaves the conflicts in the worktree.
# The agent only resolves and commits.
if [ -n "${SPIN_GIT_MERGE:-}" ] && [ -z "$(git rev-parse -q --verify MERGE_HEAD 2>/dev/null)" ]; then
  git fetch -q --depth=200 origin "+refs/heads/${SPIN_GIT_MERGE}:refs/remotes/origin/${SPIN_GIT_MERGE}" 2>/dev/null || true
  SPIN_DEEPEN=0
  while [ "$SPIN_DEEPEN" -lt 4 ] && ! git merge-base "refs/remotes/origin/${SPIN_GIT_MERGE}" HEAD >/dev/null 2>&1; do
    git fetch -q --deepen=200 origin \
      "+refs/heads/${SPIN_GIT_MERGE}:refs/remotes/origin/${SPIN_GIT_MERGE}" \
      "+refs/heads/${SPIN_GIT_HEAD}:refs/remotes/origin/${SPIN_GIT_HEAD}" 2>/dev/null || break
    SPIN_DEEPEN=$((SPIN_DEEPEN+1))
  done
  if git merge-base "refs/remotes/origin/${SPIN_GIT_MERGE}" HEAD >/dev/null 2>&1; then
    git -c user.name="${SPIN_GIT_AUTHOR_NAME:-Spin}" -c user.email="${SPIN_GIT_AUTHOR_EMAIL:-spin@local.invalid}" \
      merge --no-commit --no-ff "refs/remotes/origin/${SPIN_GIT_MERGE}" >/dev/null 2>&1 || true
  fi
fi
git config spin.targetRef "$SPIN_GIT_TARGET"
if ! git config --get spin.baseCommit >/dev/null 2>&1; then
  SPIN_INITIAL_HEAD="$(git reflog show --format=%H "$SPIN_GIT_HEAD" | tail -n 1)"
  git config spin.baseCommit "${SPIN_INITIAL_HEAD:-$(git rev-parse HEAD)}"
fi
if [ -n "$SPIN_GIT_AUTHOR_NAME" ]; then git config user.name "$SPIN_GIT_AUTHOR_NAME"; fi
if [ -n "$SPIN_GIT_AUTHOR_EMAIL" ]; then git config user.email "$SPIN_GIT_AUTHOR_EMAIL"; fi
unset SPIN_GIT_PASSWORD