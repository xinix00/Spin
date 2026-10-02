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
fi
git fetch -q --depth=1 origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}"
git checkout -q -B "$SPIN_GIT_BASE" "refs/remotes/origin/${SPIN_GIT_BASE}"
git remote set-url --push origin no_push
git config spin.reference 1
unset SPIN_GIT_PASSWORD