#!/usr/bin/env bash
# Installs the latest `harness` release binary for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/BibhabenduMukherjee/HiveMind/main/install.sh | bash
#
# Override install location with HARNESS_INSTALL_DIR (default: ~/.local/bin).
set -euo pipefail

REPO="BibhabenduMukherjee/HiveMind"
BIN_NAME="harness"
INSTALL_DIR="${HARNESS_INSTALL_DIR:-$HOME/.local/bin}"

# Global (not `local`) on purpose: a `local` var set inside main() goes out
# of scope the moment main() returns, and this EXIT trap fires *after* that
# return — under `set -u` that turned into "tmp: unbound variable" on every
# successful run (caught by actually running the installer end-to-end).
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

os_component() {
  case "$(uname -s)" in
    Darwin) echo "apple-darwin" ;;
    Linux) echo "unknown-linux-gnu" ;;
    *)
      echo "error: unsupported OS: $(uname -s) (build from source instead: cargo build --release -p harness-cli)" >&2
      exit 1
      ;;
  esac
}

arch_component() {
  case "$(uname -m)" in
    x86_64 | amd64) echo "x86_64" ;;
    arm64 | aarch64) echo "aarch64" ;;
    *)
      echo "error: unsupported architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

main() {
  local target="$(arch_component)-$(os_component)"

  echo "Looking up the latest release..."
  local latest
  latest=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
  if [ -z "${latest:-}" ]; then
    echo "error: could not determine the latest release of ${REPO}" >&2
    exit 1
  fi

  local archive="${BIN_NAME}-${target}.tar.gz"
  local url="https://github.com/${REPO}/releases/download/${latest}/${archive}"

  echo "Installing ${BIN_NAME} ${latest} for ${target}..."

  if ! curl -fsSL "$url" -o "$TMP_DIR/$archive"; then
    echo "error: no prebuilt binary for ${target} in release ${latest}." >&2
    echo "Build from source instead: cargo build --release -p harness-cli" >&2
    exit 1
  fi
  tar -xzf "$TMP_DIR/$archive" -C "$TMP_DIR"

  mkdir -p "$INSTALL_DIR"
  mv "$TMP_DIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
  chmod +x "$INSTALL_DIR/$BIN_NAME"

  echo "Installed to $INSTALL_DIR/$BIN_NAME"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      echo
      echo "Add this to your shell profile, then restart your shell:"
      echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
      ;;
  esac

  echo
  echo "Next:"
  echo "  export DEEPSEEK_API_KEY=sk-..."
  echo "  $BIN_NAME"
}

main "$@"
