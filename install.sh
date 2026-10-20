#!/bin/sh
# CSG Scion installer: the client `scn`, and with --server the server `scion` as a service.
#
#   curl -fsSL https://raw.githubusercontent.com/OpenCSGs/scion/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/OpenCSGs/scion/main/install.sh | sh -s -- --server
#
# Options (after `sh -s --`):
#   --server            also install `scion` and run it as a service (systemd on Linux, launchd on
#                       macOS): Lite mode, SQLite and a local directory
#   --listen ADDR       the server's address (default 127.0.0.1:7080). Any address beyond loopback
#                       turns authentication on and creates the first administrator's token
#   --data-dir DIR      the server's data (default /var/lib/scion on Linux,
#                       ~/Library/Application Support/scion on macOS)
#   --no-service        install the binaries only, without registering or starting a service
#   --version vX.Y.Z    a release (default: the latest)
#   --install-dir DIR   where the binaries go (default /usr/local/bin, else ~/.local/bin)
#
# Environment: SCION_VERSION, SCION_INSTALL_DIR, and SCION_DOWNLOAD_URL for a mirror of the
# release files (default https://github.com/OpenCSGs/scion/releases/download).
set -eu

REPO="OpenCSGs/scion"
DOWNLOAD_URL="${SCION_DOWNLOAD_URL:-https://github.com/${REPO}/releases/download}"
VERSION="${SCION_VERSION:-}"
INSTALL_DIR="${SCION_INSTALL_DIR:-}"
SERVER=""
SERVICE=1
LISTEN="127.0.0.1:7080"
DATA_DIR=""
LABEL="com.opencsg.scion"

if [ -t 1 ]; then
    GREEN='\033[0;32m' YELLOW='\033[1;33m' RED='\033[0;31m' CYAN='\033[0;36m' BOLD='\033[1m' NC='\033[0m'
else
    GREEN='' YELLOW='' RED='' CYAN='' BOLD='' NC=''
fi
info() { printf "${GREEN}[INFO]${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$1" >&2; exit 1; }

usage() {
    cat <<'USAGE'
CSG Scion installer: the client scn, and with --server the server scion as a service.

  curl -fsSL https://raw.githubusercontent.com/OpenCSGs/scion/main/install.sh | sh -s -- [OPTIONS]

  --server            also install scion and run it as a service (systemd on Linux, launchd on macOS)
  --listen ADDR       the server's address (default 127.0.0.1:7080); beyond loopback, authentication
                      is turned on and the first administrator's token is created
  --data-dir DIR      the server's data (default /var/lib/scion on Linux,
                      ~/Library/Application Support/scion on macOS)
  --no-service        install the binaries only
  --version vX.Y.Z    a release (default: the latest)
  --install-dir DIR   where the binaries go

Environment: SCION_VERSION, SCION_INSTALL_DIR, SCION_DOWNLOAD_URL (a mirror of the release files).
USAGE
    exit 0
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --server) SERVER=1 ;;
            --no-service) SERVICE="" ;;
            --listen) [ $# -ge 2 ] || error "--listen needs an address"; LISTEN="$2"; shift ;;
            --data-dir) [ $# -ge 2 ] || error "--data-dir needs a directory"; DATA_DIR="$2"; shift ;;
            --version) [ $# -ge 2 ] || error "--version needs a tag"; VERSION="$2"; shift ;;
            --install-dir) [ $# -ge 2 ] || error "--install-dir needs a directory"; INSTALL_DIR="$2"; shift ;;
            -h|--help) usage ;;
            *) error "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
}

need() { command -v "$1" >/dev/null 2>&1 || error "$1 is required"; }

# Run as root: directly when we are root, else through sudo
as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"
    elif command -v sudo >/dev/null 2>&1; then sudo "$@"
    else error "root is needed for: $* (run as root, or install sudo)"
    fi
}

detect_target() {
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) error "unsupported architecture: $(uname -m)" ;;
    esac
    case "$(uname -s)" in
        Linux) OS=linux; TARGET="${arch}-unknown-linux-gnu"; check_glibc ;;
        Darwin) OS=darwin; TARGET="${arch}-apple-darwin" ;;
        *) error "unsupported system: $(uname -s); see https://github.com/${REPO}#deployment for Docker" ;;
    esac
}

# The Linux builds need glibc 2.35 or newer (Ubuntu 22.04, Debian 12, RHEL 10)
check_glibc() {
    v="$(ldd --version 2>/dev/null | head -1 | grep -Eo '[0-9]+\.[0-9]+$' || true)"
    if [ -z "$v" ]; then
        warn "could not find glibc (musl, such as Alpine, is not supported); the Docker image is an alternative"
        return
    fi
    major="${v%.*}" minor="${v#*.}"
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
        error "glibc $v is too old: the binaries need 2.35 or newer; the Docker image is an alternative"
    fi
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --connect-timeout 15 --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --timeout=15 --tries=3 -O "$2" "$1"
    else
        error "curl or wget is required"
    fi
}

# The latest release's tag, from where /releases/latest redirects (no API rate limit)
latest_version() {
    url="https://github.com/${REPO}/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        loc="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url" 2>/dev/null || true)"
    else
        loc="$(wget -S --spider "$url" 2>&1 | sed -n 's/^ *Location: *//p' | tail -1 || true)"
    fi
    tag="${loc##*/}"
    case "$tag" in v[0-9]*) printf '%s\n' "$tag" ;; *) return 1 ;; esac
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Download a package, check it against SHA256SUMS and unpack it into $TMP
fetch() {
    name="$1-${VERSION}-${TARGET}"
    info "downloading ${name}.tar.gz"
    download "${DOWNLOAD_URL}/${VERSION}/${name}.tar.gz" "${TMP}/${name}.tar.gz" || error "download failed: ${DOWNLOAD_URL}/${VERSION}/${name}.tar.gz"
    want="$(grep " ${name}.tar.gz\$" "${TMP}/SHA256SUMS" | cut -d' ' -f1)"
    [ -n "$want" ] || error "${name}.tar.gz is not in SHA256SUMS"
    [ "$(sha256 "${TMP}/${name}.tar.gz")" = "$want" ] || error "checksum mismatch for ${name}.tar.gz"
    tar -xzf "${TMP}/${name}.tar.gz" -C "$TMP"
}

resolve_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        mkdir -p "$INSTALL_DIR" 2>/dev/null || as_root mkdir -p "$INSTALL_DIR"
        return
    fi
    # /usr/local/bin when it is writable, or for a server (a service runs from a system
    # directory); else a directory of the user's already on PATH; else /usr/local/bin through
    # sudo; else ~/.local/bin, with a hint about PATH
    INSTALL_DIR=/usr/local/bin
    if [ -z "$SERVER" ] && ! [ -w /usr/local/bin ] && [ "$(id -u)" != 0 ]; then
        for d in "${HOME}/.local/bin" "${HOME}/bin"; do
            case ":${PATH}:" in *":${d}:"*) INSTALL_DIR="$d"; break ;; esac
        done
        if [ "$INSTALL_DIR" = /usr/local/bin ] && ! command -v sudo >/dev/null 2>&1; then
            INSTALL_DIR="${HOME}/.local/bin"
        fi
    fi
    [ -d "$INSTALL_DIR" ] || mkdir -p "$INSTALL_DIR" 2>/dev/null || as_root mkdir -p "$INSTALL_DIR"
}

install_bin() {
    src="$1" dest="${INSTALL_DIR}/$(basename "$1")"
    chmod 755 "$src"
    if [ -w "$INSTALL_DIR" ]; then mv -f "$src" "$dest"; else as_root mv -f "$src" "$dest"; fi
    info "installed $dest"
}

is_loopback() {
    case "$1" in 127.*|localhost:*|"[::1]":*) return 0 ;; *) return 1 ;; esac
}

# The first administrator's policy: admins manage everything; anyone signed in can read, write and land
write_policy() {
    printf '%s\n' '[[rule]]' 'subjects = ["group:admins"]' 'repos = ["*"]' 'actions = ["admin"]' \
        '[[rule]]' 'subjects = ["*"]' 'repos = ["*"]' 'actions = ["land"]' > "$1"
}

server_args() {
    printf '%s' "--log-format json server --lite --data-dir \"${DATA_DIR}\" --listen ${LISTEN}"
    is_loopback "$LISTEN" || printf '%s' " --auth-required"
}

# Bootstrap authentication: a policy and a token, once (an existing policy is kept). Runs the
# admin commands as $1 (a user, or empty for the current one).
bootstrap_auth() {
    run_as="$1"
    is_loopback "$LISTEN" && return 0
    admin() {
        if [ -z "$run_as" ]; then "${INSTALL_DIR}/scion" "$@"
        elif command -v runuser >/dev/null 2>&1; then as_root runuser -u "$run_as" -- "${INSTALL_DIR}/scion" "$@"
        else as_root sudo -u "$run_as" "${INSTALL_DIR}/scion" "$@"
        fi
    }
    if admin admin policy show --lite --data-dir "$DATA_DIR" 2>/dev/null | grep -q '\[\[rule\]\]'; then
        info "keeping the existing access policy; to create another token:"
        if [ -n "$run_as" ]; then printf '  sudo -u %s ' "$run_as"; else printf '  '; fi
        printf '%s/scion admin token create --lite --data-dir "%s" --subject NAME\n' "$INSTALL_DIR" "$DATA_DIR"
        return 0
    fi
    write_policy "${TMP}/acl.toml"
    chmod 644 "${TMP}/acl.toml"
    chmod 755 "$TMP"
    admin admin policy set --lite --data-dir "$DATA_DIR" "${TMP}/acl.toml" >/dev/null
    TOKEN="$(admin admin token create --lite --data-dir "$DATA_DIR" --subject admin --group admins --description "created by install.sh" | sed -n 's/^token: *//p')"
}

service_linux() {
    if ! command -v systemctl >/dev/null 2>&1 || ! [ -d /run/systemd/system ]; then
        warn "systemd is not running here, so no service was registered. Start the server with:"
        printf '  %s/scion %s\n' "$INSTALL_DIR" "$(server_args)"
        return 0
    fi
    DATA_DIR="${DATA_DIR:-/var/lib/scion}"
    id scion >/dev/null 2>&1 || as_root useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin scion 2>/dev/null \
        || as_root useradd --system --home-dir "$DATA_DIR" --shell /sbin/nologin scion
    as_root mkdir -p "$DATA_DIR"
    as_root chown scion "$DATA_DIR"
    bootstrap_auth scion
    unit="${TMP}/scion.service"
    cat > "$unit" <<EOF
[Unit]
Description=CSG Scion server
After=network-online.target
Wants=network-online.target

[Service]
User=scion
Environment=SCION_NO_DAEMON=1
ExecStart=${INSTALL_DIR}/scion $(server_args)
Restart=on-failure
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
    as_root install -m 644 "$unit" /etc/systemd/system/scion.service
    as_root systemctl daemon-reload
    as_root systemctl enable scion >/dev/null 2>&1
    as_root systemctl restart scion
    info "service scion started (systemctl status scion; journalctl -u scion)"
    STOP_HINT="sudo systemctl stop scion"
}

service_darwin() {
    DATA_DIR="${DATA_DIR:-${HOME}/Library/Application Support/scion}"
    mkdir -p "$DATA_DIR" "${HOME}/Library/LaunchAgents" "${HOME}/Library/Logs"
    bootstrap_auth ""
    plist="${HOME}/Library/LaunchAgents/${LABEL}.plist"
    args=""
    for a in --log-format json server --lite --data-dir "$DATA_DIR" --listen "$LISTEN"; do
        args="${args}        <string>${a}</string>
"
    done
    is_loopback "$LISTEN" || args="${args}        <string>--auth-required</string>
"
    cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${INSTALL_DIR}/scion</string>
${args}    </array>
    <key>EnvironmentVariables</key>
    <dict><key>SCION_NO_DAEMON</key><string>1</string></dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>StandardOutPath</key><string>${HOME}/Library/Logs/scion.log</string>
    <key>StandardErrorPath</key><string>${HOME}/Library/Logs/scion.log</string>
</dict>
</plist>
EOF
    launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$plist"
    info "service ${LABEL} started (logs in ~/Library/Logs/scion.log)"
    STOP_HINT="launchctl bootout gui/$(id -u)/${LABEL}"
}

wait_ready() {
    host="${LISTEN%:*}" port="${LISTEN##*:}"
    case "$host" in 0.0.0.0|"[::]"|"") host=127.0.0.1 ;; esac
    i=0
    while [ $i -lt 50 ]; do
        if SCION_NO_DAEMON=1 "${INSTALL_DIR}/scn" doctor "http://${host}:${port}" >/dev/null 2>&1; then
            info "the server answers on http://${host}:${port}"
            return 0
        fi
        i=$((i + 1)); sleep 0.2
    done
    warn "the server did not answer on http://${host}:${port} yet; check its logs"
}

main() {
    parse_args "$@"
    need uname; need tar
    detect_target
    if [ -z "$VERSION" ]; then
        VERSION="$(latest_version)" || error "could not find the latest release; pass --version vX.Y.Z"
    fi
    printf "\n${BOLD}Installing CSG Scion %s (%s)${NC}\n\n" "$VERSION" "$TARGET"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    download "${DOWNLOAD_URL}/${VERSION}/SHA256SUMS" "${TMP}/SHA256SUMS" || error "no release ${VERSION} at ${DOWNLOAD_URL}"
    resolve_install_dir
    fetch scn
    install_bin "${TMP}/scn-${VERSION}-${TARGET}/scn"
    TOKEN="" STOP_HINT=""
    if [ -n "$SERVER" ]; then
        fetch scion-server
        install_bin "${TMP}/scion-server-${VERSION}-${TARGET}/scion"
        if [ -n "$SERVICE" ]; then
            if [ "$OS" = linux ]; then service_linux; else service_darwin; fi
            [ -z "$STOP_HINT" ] || wait_ready
        else
            DATA_DIR="${DATA_DIR:-${HOME}/.scion-data}"
            info "start the server with: ${INSTALL_DIR}/scion $(server_args)"
        fi
    fi

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) warn "${INSTALL_DIR} is not on PATH; add it: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
    esac

    host="${LISTEN%:*}"
    case "$host" in 0.0.0.0|"[::]") host="$(hostname 2>/dev/null || echo localhost)" ;; esac
    url="http://${host}:${LISTEN##*:}"
    printf "\n${GREEN}${BOLD}CSG Scion %s is installed.${NC}\n\n${BOLD}Quick start:${NC}\n" "$VERSION"
    if [ -n "$TOKEN" ]; then
        printf "  The first administrator's token, shown only this once:\n    ${CYAN}%s${NC}\n" "$TOKEN"
        printf "  scn login %s --token %s\n" "$url" "$TOKEN"
    fi
    if [ -n "$SERVER" ] || [ -n "$TOKEN" ]; then
        printf "  scn repo create %s/mono\n  scn clone %s/mono && cd mono\n" "$url" "$url"
    else
        printf "  scn clone http://SERVER:7080/REPO && cd REPO\n"
    fi
    printf "  scn --help\n"
    [ -z "$STOP_HINT" ] || printf "\n  Stop the server: %s\n" "$STOP_HINT"
    printf "\n  Docs: https://github.com/%s\n\n" "$REPO"
}

main "$@"
