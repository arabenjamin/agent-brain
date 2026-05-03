#!/bin/bash
# Agent Brain entrypoint — configures git identity + SSH signing key, then starts the server.
set -e

SSH_DIR="/home/agent/.ssh"
SSH_KEY="${SSH_DIR}/id_ed25519"

# ---------------------------------------------------------------------------
# 1. SSH signing key — generate once, persist via volume.
# ---------------------------------------------------------------------------
mkdir -p "${SSH_DIR}"
chmod 700 "${SSH_DIR}"

if [ ! -f "${SSH_KEY}" ]; then
    ssh-keygen -t ed25519 -f "${SSH_KEY}" -N "" \
        -C "agent-brain@$(hostname)" > /dev/null 2>&1
    echo "========================================================"
    echo "  agent-brain: new signing key generated."
    echo ""
    echo "  Add this public key to GitHub in TWO places:"
    echo "  1. Account Settings → SSH and GPG keys → New signing key"
    echo "     (so GitHub shows commits as 'Verified')"
    echo "  2. The agent-brain repo → Settings → Deploy keys → Add key"
    echo "     (so the brain can push branches without a token)"
    echo ""
    echo "  Public key:"
    cat "${SSH_KEY}.pub"
    echo "========================================================"
fi

# Write allowed-signers file so local 'git log --show-signature' works.
ALLOWED_SIGNERS="${SSH_DIR}/allowed_signers"
GIT_EMAIL="${GIT_USER_EMAIL:-agent-brain@noreply.local}"
echo "${GIT_EMAIL} $(cat ${SSH_KEY}.pub)" > "${ALLOWED_SIGNERS}"

# ---------------------------------------------------------------------------
# 2. Git identity.
# ---------------------------------------------------------------------------
git config --global user.name  "${GIT_USER_NAME:-agent-brain}"
git config --global user.email "${GIT_EMAIL}"

# ---------------------------------------------------------------------------
# 3. SSH commit signing.
# ---------------------------------------------------------------------------
git config --global gpg.format          ssh
git config --global user.signingKey     "${SSH_KEY}.pub"
git config --global gpg.ssh.allowedSignersFile "${ALLOWED_SIGNERS}"
git config --global commit.gpgSign      true

# ---------------------------------------------------------------------------
# 4. HTTPS push authentication via GITHUB_TOKEN (fallback when not using
#    the deploy key). Maps git@github.com: → https://github.com/ so both
#    SSH-format and HTTPS-format remotes work with the same token.
# ---------------------------------------------------------------------------
if [ -n "${GITHUB_TOKEN}" ]; then
    git config --global \
        url."https://git:${GITHUB_TOKEN}@github.com/".insteadOf \
        "git@github.com:"
    git config --global \
        url."https://git:${GITHUB_TOKEN}@github.com/".insteadOf \
        "https://github.com/"
fi

# ---------------------------------------------------------------------------
# 5. Trust the codebase directory (avoids "dubious ownership" errors when
#    the volume is owned by a different UID on the host).
# ---------------------------------------------------------------------------
git config --global --add safe.directory "/home/agent/agent-brain"
if [ -n "${CODEBASE_DIR}" ] && [ "${CODEBASE_DIR}" != "/home/agent/agent-brain" ]; then
    git config --global --add safe.directory "${CODEBASE_DIR}"
fi

# ---------------------------------------------------------------------------
# 6. Start the server.
# ---------------------------------------------------------------------------
exec agent-brain "$@"
