#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="${HOME}/.local/bin"

mkdir -p "${DEST_DIR}"

cat > "${DEST_DIR}/meter" <<EOF
#!/usr/bin/env bash
set -euo pipefail
REPO_DIR="${REPO_DIR}"
exec "\${REPO_DIR}/bin/meter" "\$@"
EOF

cat > "${DEST_DIR}/meter-build" <<EOF
#!/usr/bin/env bash
set -euo pipefail
REPO_DIR="${REPO_DIR}"
exec "\${REPO_DIR}/bin/meter-build" "\$@"
EOF

chmod +x "${DEST_DIR}/meter" "${DEST_DIR}/meter-build"

cat <<MSG
Installed:
  ${DEST_DIR}/meter
  ${DEST_DIR}/meter-build

Next:
  1) Build once: meter-build
  2) Run: meter

If 'meter' is not found in your current shell, reload your shell config:
  source ~/.zshrc
MSG
