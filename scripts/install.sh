#!/usr/bin/env sh
# Install memoria binary from GitHub releases.
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | sh
#   curl -sSL ... | sh -s -- -v v0.1.0-rc1
#   curl -sSL ... | sh -s -- -y              # skip confirmation
#   curl -sSL ... | sh -s -- -d ~/.local/bin  # custom directory
#   curl -sSL ... | sh -s -- --no-telemetry   # disable install telemetry
#
# Options:
#   -v, --version TAG      Version to install (default: latest release)
#   -d, --dir DIR          Install directory (default: /usr/local/bin, sudo if needed)
#   -y, --yes              Skip confirmation prompt
#   -n, --dry-run          Print download URL and exit
#   --no-telemetry         Disable anonymous install telemetry
#
# Env:
#   MEMORIA_REPO           GitHub repo (default: matrixorigin/Memoria)
#   MEMORIA_VERSION        Version tag (default: latest)
#   MEMORIA_GHPROXY        Optional HTTPS mirror base URL (disabled by default)
#   MEMORIA_NO_TELEMETRY   Set to 1 to disable install telemetry

set -eu

# ── Colors ──────────────────────────────────────────────────────────

BOLD="$(tput bold 2>/dev/null || printf '')"
GREEN="$(tput setaf 2 2>/dev/null || printf '')"
YELLOW="$(tput setaf 3 2>/dev/null || printf '')"
BLUE="$(tput setaf 4 2>/dev/null || printf '')"
RED="$(tput setaf 1 2>/dev/null || printf '')"
NC="$(tput sgr0 2>/dev/null || printf '')"

info()  { printf '%s\n' "${BOLD}>${NC} $*"; }
warn()  { printf '%s\n' "${YELLOW}! $*${NC}"; }
error() { printf '%s\n' "${RED}x $*${NC}" >&2; }
ok()    { printf '%s\n' "${GREEN}✓${NC} $*"; }

# ── Banner ──────────────────────────────────────────────────────────

cat << "EOF"

███╗   ███╗███████╗███╗   ███╗ ██████╗ ██████╗ ██╗ █████╗ 
████╗ ████║██╔════╝████╗ ████║██╔═══██╗██╔══██╗██║██╔══██╗
██╔████╔██║█████╗  ██╔████╔██║██║   ██║██████╔╝██║███████║
██║╚██╔╝██║██╔══╝  ██║╚██╔╝██║██║   ██║██╔══██╗██║██╔══██║
██║ ╚═╝ ██║███████╗██║ ╚═╝ ██║╚██████╔╝██║  ██║██║██║  ██║
╚═╝     ╚═╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝
            Memoria - Secure · Auditable · Programmable Memory
EOF

# ── Prerequisites ───────────────────────────────────────────────────

if ! command -v curl >/dev/null 2>&1; then
  error "curl is required but not found"
  exit 1
fi

# ── Defaults ────────────────────────────────────────────────────────

REPO="${MEMORIA_REPO:-matrixorigin/Memoria}"
VERSION="${MEMORIA_VERSION:-}"
INSTALL_DIR=""
DRY_RUN=false
FORCE=false
NO_TELEMETRY="${MEMORIA_NO_TELEMETRY:-0}"

# ── Platform detection ──────────────────────────────────────────────

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="" ;;
  esac
  case "$os" in
    linux)
      [ "$arch" = "x86_64" ] && printf "x86_64-unknown-linux-musl" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-unknown-linux-musl" && return
      ;;
    darwin)
      [ "$arch" = "x86_64" ] && printf "x86_64-apple-darwin" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-apple-darwin" && return
      ;;
  esac
  printf ""
}

# ── Writability test ────────────────────────────────────────────────

test_writeable() {
  path="${1}/._memoria_write_test"
  if touch "${path}" 2>/dev/null; then
    rm -f "${path}"
    return 0
  fi
  return 1
}

# ── Sudo elevation ─────────────────────────────────────────────────

elevate_priv() {
  if ! command -v sudo >/dev/null 2>&1; then
    error "Need write access to ${INSTALL_DIR} but 'sudo' not found"
    info "Either run as root, or use: -d ~/.local/bin"
    exit 1
  fi
  warn "Elevated permissions required to install to ${INSTALL_DIR}"
  if ! sudo -v; then
    error "Superuser not granted, aborting"
    exit 1
  fi
}

# ── PATH detection & shell config hints ─────────────────────────────

check_path() {
  dir="$1"
  case ":${PATH}:" in
    *:"${dir}":*) return 0 ;;
  esac
  return 1
}

print_path_hint() {
  dir="$1"
  printf '\n'
  warn "${dir} is not in your PATH"
  info "Add it by running one of:"
  printf '\n'

  shell_name="$(basename "${SHELL:-sh}")"
  case "$shell_name" in
    zsh)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc && source ~/.zshrc${NC}"
      ;;
    bash)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc && source ~/.bashrc${NC}"
      ;;
    fish)
      info "  ${BLUE}fish_add_path ${dir}${NC}"
      ;;
    *)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc${NC}  (bash)"
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc${NC}   (zsh)"
      info "  ${BLUE}fish_add_path ${dir}${NC}                              (fish)"
      ;;
  esac
  printf '\n'
  info "Then run ${BLUE}memoria init -i${NC} in your project directory to start the setup wizard"
}

# ── Confirmation ────────────────────────────────────────────────────

confirm() {
  if [ "$FORCE" = true ]; then return 0; fi
  printf "%s " "$* ${BOLD}[y/N]${NC}"
  read -r yn < /dev/tty || return 1
  case "$yn" in
    [Yy]*) return 0 ;;
    *) return 1 ;;
  esac
}

INIT_TOOL=""
INIT_API_URL=""
INIT_TOKEN=""

# ── Parse args ──────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
    -v|--version)      VERSION="$2"; shift 2 ;;
    -d|--dir)          INSTALL_DIR="$2"; shift 2 ;;
    -y|--yes)          FORCE=true; shift ;;
    -n|--dry-run)      DRY_RUN=true; shift ;;
    --no-telemetry)    NO_TELEMETRY=1; shift ;;
    --tool)            INIT_TOOL="$2"; shift 2 ;;
    --api-url)         INIT_API_URL="$2"; shift 2 ;;
    --token)           INIT_TOKEN="$2"; shift 2 ;;
    -h|--help)
      printf "Usage: install.sh [options]\n\n"
      printf "  -v, --version TAG      Version to install (default: latest)\n"
      printf "  -d, --dir DIR          Install directory (default: /usr/local/bin)\n"
      printf "  -y, --yes              Skip confirmation prompt\n"
      printf "  -n, --dry-run          Print download URL and exit\n"
      printf "  --no-telemetry         Disable anonymous install telemetry\n"
      printf "  --tool TOOL            Auto-init after install (kiro|cursor|claude|codex)\n"
      printf "  --api-url URL          Memoria API URL for auto-init\n"
      printf "  --token TOKEN          Memoria API token for auto-init\n"
      printf "  -h, --help             Show this help\n"
      exit 0
      ;;
    *) shift ;;
  esac
done

# ── Resolve target & URL ────────────────────────────────────────────

TARGET=$(detect_target)
if [ -z "$TARGET" ]; then
  error "Unsupported platform: $(uname -s) $(uname -m)"
  exit 1
fi

TAG="${VERSION:-latest}"
if [ "$TAG" = "latest" ]; then
  RESOLVED_TAG=$(curl -sSf -o /dev/null -w '%{redirect_url}' "https://github.com/${REPO}/releases/latest" 2>/dev/null | grep -oE '[^/]+$')
  if [ -n "$RESOLVED_TAG" ]; then
    TAG="$RESOLVED_TAG"
  fi
fi
ASSET="memoria-${TARGET}.tar.gz"
GH_URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
GH_SUM_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
# Mirrors are opt-in, never an implicit change of the release source.
GHPROXY="${MEMORIA_GHPROXY:-}"
if [ -n "$GHPROXY" ]; then
  case "$GHPROXY" in
    https://*) GHPROXY="${GHPROXY%/}" ;;
    *) error "MEMORIA_GHPROXY must be an HTTPS URL"; exit 1 ;;
  esac
fi

if [ "$DRY_RUN" = true ]; then
  echo "URL: $GH_URL"
  echo "Checksum URL: $GH_SUM_URL"
  echo "Mirror: ${GHPROXY:-disabled (set MEMORIA_GHPROXY to enable)}"
  exit 0
fi

# ── Resolve install directory ───────────────────────────────────────

if [ -z "$INSTALL_DIR" ]; then
  INSTALL_DIR=/usr/local/bin
fi

# ── Check existing installation ─────────────────────────────────────

SKIP_DOWNLOAD=false
# Auto-confirm when all init params are provided
if [ -n "$INIT_TOOL" ] && [ -n "$INIT_API_URL" ] && [ -n "$INIT_TOKEN" ]; then
  FORCE=true
fi
if command -v memoria >/dev/null 2>&1; then
  INSTALLED_VERSION="$(memoria --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
  if [ -n "$INSTALLED_VERSION" ]; then
    TARGET_VERSION="${TAG#v}"
    if [ "$INSTALLED_VERSION" = "$TARGET_VERSION" ]; then
      ok "memoria v${INSTALLED_VERSION} already installed (latest)"
      SKIP_DOWNLOAD=true
      INSTALL_DIR="$(dirname "$(command -v memoria)")"
    else
      info "memoria v${INSTALLED_VERSION} installed, upgrading to v${TARGET_VERSION}"
    fi
  fi
fi

# ── Show plan & confirm ────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then
  printf '\n'
  info "${BOLD}Version${NC}:   ${GREEN}${TAG}${NC}"
  info "${BOLD}Platform${NC}:  ${GREEN}${TARGET}${NC}"
  info "${BOLD}Directory${NC}: ${GREEN}${INSTALL_DIR}${NC}"
  printf '\n'

  if ! confirm "Install memoria?"; then
    info "Aborted"
    exit 0
  fi
fi

# ── Download ────────────────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then

# Determine sudo requirement
SUDO=""
if ! test_writeable "$INSTALL_DIR" 2>/dev/null; then
  if [ ! -d "$INSTALL_DIR" ]; then
    if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
      elevate_priv
      SUDO="sudo"
      $SUDO mkdir -p "$INSTALL_DIR"
    fi
  else
    elevate_priv
    SUDO="sudo"
  fi
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

info "Downloading ${BLUE}${GH_URL}${NC}"
# Limit connection setup, not the total time needed to download a release.
if ! curl -fL# --connect-timeout 10 --retry 2 -o "$TMP/$ASSET" "$GH_URL" 2>/dev/null; then
  if [ -z "$GHPROXY" ]; then
    error "Direct download failed; set MEMORIA_GHPROXY to opt in to an HTTPS mirror"
    exit 1
  fi
  warn "Direct download failed; using explicitly configured mirror: ${GHPROXY}"
  URL="${GHPROXY}/${GH_URL}"
  info "Downloading ${BLUE}${URL}${NC}"
  curl -fL# --connect-timeout 10 --retry 2 -o "$TMP/$ASSET" "$URL" || {
    error "Download failed — check that version '${TAG}' exists"
    info "Available releases: ${BLUE}https://github.com/${REPO}/releases${NC}"
    exit 1
  }
fi

# ── Verify checksum ─────────────────────────────────────────────────

# Never trust a mirror's checksum: fetch it exclusively from GitHub over TLS.
# If the official checksum is unavailable, installation MUST fail closed.
if ! curl -sSLf --connect-timeout 10 --retry 2 -o "$TMP/SHA256SUMS.txt" "$GH_SUM_URL" 2>/dev/null; then
  error "Cannot fetch official SHA256SUMS.txt from GitHub; refusing to install"
  exit 1
fi
if (cd "$TMP" && grep -F "$ASSET" SHA256SUMS.txt | (sha256sum -c 2>/dev/null || shasum -a 256 -c 2>/dev/null)); then
  ok "Checksum verified"
else
  error "Checksum verification failed"
  exit 1
fi

# ── Install ─────────────────────────────────────────────────────────

tar -xzf "$TMP/$ASSET" -C "$TMP"
$SUDO rm -f "$INSTALL_DIR/memoria"
$SUDO cp "$TMP/memoria" "$INSTALL_DIR/memoria"
$SUDO chmod +x "$INSTALL_DIR/memoria"

printf '\n'
ok "Installed ${GREEN}memoria${NC} to ${GREEN}${INSTALL_DIR}/memoria${NC}"

# ── Telemetry ────────────────────────────────────────────────────────────────
# Sends a single anonymous ping to thememoria.ai after a successful install.
# Collected: tool name, version, OS/arch platform. No personal data or tokens.
# Disable with: --no-telemetry flag or MEMORIA_NO_TELEMETRY=1 environment variable.

_track_install() {
  if [ "$NO_TELEMETRY" = "1" ]; then
    return
  fi
  _tool="${INIT_TOOL:-}"
  _ver="${TAG:-}"
  _plat="${TARGET:-}"
  _api="${INIT_API_URL:-}"
  _payload="{\"tool\":\"${_tool}\",\"version\":\"${_ver}\",\"platform\":\"${_plat}\",\"api_url\":\"${_api}\"}"
  # Fire-and-forget: run in background subshell, suppress all output.
  # If the server is unreachable the install is completely unaffected.
  (curl -sSf --max-time 5 -X POST \
    "https://thememoria.ai/api/track/install" \
    -H "Content-Type: application/json" \
    -d "$_payload" \
    >/dev/null 2>&1) &
}

_track_install

fi # end SKIP_DOWNLOAD

# ── Auto-init ────────────────────────────────────────────────────────

if [ -n "$INIT_TOOL" ] && [ -n "$INIT_API_URL" ] && [ -n "$INIT_TOKEN" ]; then
  printf '\n'
  info "Running: memoria init --tool ${INIT_TOOL} --api-url ${INIT_API_URL} --token ***"
  "$INSTALL_DIR/memoria" init \
    --tool "$INIT_TOOL" \
    --api-url "$INIT_API_URL" \
    --token "$INIT_TOKEN" \
    --force
elif [ -n "$INIT_TOOL" ]; then
  printf '\n'
  info "Running: memoria init -i --tool ${INIT_TOOL}"
  "$INSTALL_DIR/memoria" init -i --tool "$INIT_TOOL"
fi

# ── PATH check ──────────────────────────────────────────────────────

if ! check_path "$INSTALL_DIR"; then
  print_path_hint "$INSTALL_DIR"
elif [ -z "$INIT_TOOL" ]; then
  printf '\n'
  info "Next: run ${BLUE}memoria init -i${NC} in your project directory to start the setup wizard"
fi
