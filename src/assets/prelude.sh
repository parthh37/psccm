# ---- PSCCM helpers (available in every bash step) ----
set -eo pipefail
export DEBIAN_FRONTEND=noninteractive
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"

# Prints apt-get, dnf, yum, zypper, apk or none.
pkg_manager() {
    for m in apt-get dnf yum zypper apk; do
        if command -v "$m" >/dev/null 2>&1; then echo "$m"; return 0; fi
    done
    echo none
}

pkg_install() {
    case "$(pkg_manager)" in
        apt-get) apt-get update -q && apt-get install -y -q "$@" ;;
        dnf) dnf install -y -q "$@" ;;
        yum) yum install -y -q "$@" ;;
        zypper) zypper --non-interactive install "$@" ;;
        apk) apk add --no-cache "$@" ;;
        *) echo "No supported package manager found" >&2; return 1 ;;
    esac
}

# Exit 0 when the package is installed (dpkg or rpm).
pkg_installed() {
    if command -v dpkg-query >/dev/null 2>&1; then
        dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q "install ok installed"
    elif command -v rpm >/dev/null 2>&1; then
        rpm -q "$1" >/dev/null 2>&1
    else
        return 1
    fi
}

# version_ge A B: exit 0 when version A >= version B.
version_ge() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

download() {
    echo "Downloading $1"
    mkdir -p "$(dirname "$2")"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

# Backup a file next to itself with a timestamp; prints the backup path.
backup_file() {
    local b
    b="$1.psccm-$(date +%Y%m%d%H%M%S)"
    cp -a "$1" "$b"
    echo "$b"
}
# ---- end of helpers ----
