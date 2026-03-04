#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="${HOME}/.local/bin"

mkdir -p "${DEST_DIR}"

cat > "${DEST_DIR}/meter" <<EOF2
#!/usr/bin/env bash
set -euo pipefail
REPO_DIR="${REPO_DIR}"
exec "\${REPO_DIR}/bin/meter" "\$@"
EOF2

chmod +x "${DEST_DIR}/meter"
rm -f "${DEST_DIR}/meter-build"

cat <<MSG
Installed:
  ${DEST_DIR}/meter

Run:
  meter

If 'meter' is not found in your current shell, reload your shell config:
  source ~/.zshrc
MSG
