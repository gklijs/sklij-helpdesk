#!/usr/bin/env bash
set -u
export PATH="$HOME/.cargo/bin:$PATH"
cd /home/gklijs/projects/skilj-helpdesk
rm -f tmp-who.sh
echo "=== remotes ==="
git remote -v || echo "(no remotes)"
echo "=== gpg config ==="
git config gpg.format || echo "(no gpg.format)"
git config user.signingkey || echo "(no signingkey)"
echo "=== merge commit parents ==="
git log --format="%H parents:%P %s" -2
echo "=== gh auth ==="
gh auth status 2>&1 | head -5
