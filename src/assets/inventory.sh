# PSCCM inventory scan (Linux). Prints one "@@inv<TAB>kind<TAB>fields..." line per item.
set +e +o pipefail

inv() {
    local out="@@inv	$1" f
    shift
    for f in "$@"; do
        f=${f//$'\t'/ }
        f=${f//$'\n'/ }
        out+="	$f"
    done
    printf '%s\n' "$out"
}

fact() {
    if [ -n "$2" ]; then inv fact "$1" "$2"; fi
}

dmi() {
    cat "/sys/class/dmi/id/$1" 2>/dev/null | head -n1
}

if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    fact os_name "${PRETTY_NAME:-$NAME}"
    fact os_version "$VERSION_ID"
fi
fact kernel "$(uname -r)"
fact arch "$(uname -m)"
fact manufacturer "$(dmi sys_vendor)"
fact model "$(dmi product_name)"
fact serial "$(dmi product_serial)"
fact cpu "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//')"
fact cpu_count "$(nproc 2>/dev/null || grep -c ^processor /proc/cpuinfo)"
fact ram_mb "$(awk '/^MemTotal:/ {print int($2 / 1024)}' /proc/meminfo 2>/dev/null)"
up=$(cut -d. -f1 /proc/uptime 2>/dev/null)
if [ -n "$up" ]; then
    fact last_boot "$(date -u -d "@$(( $(date +%s) - up ))" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)"
fi
fact domain "$(hostname -d 2>/dev/null)"
fact user "$(who 2>/dev/null | awk '{print $1}' | sort -u | paste -sd, - | sed 's/,/, /g')"
if command -v ip >/dev/null 2>&1; then
    fact ip_addresses "$(ip -4 -o addr show scope global 2>/dev/null | awk '{split($4, a, "/"); print a[1]}' | paste -sd, - | sed 's/,/, /g')"
else
    fact ip_addresses "$(hostname -I 2>/dev/null | xargs | sed 's/ /, /g')"
fi
fact package_manager "$(pkg_manager)"

pending=false
if [ -f /var/run/reboot-required ]; then
    pending=true
elif command -v needs-restarting >/dev/null 2>&1; then
    needs-restarting -r >/dev/null 2>&1
    [ $? -eq 1 ] && pending=true
fi
fact pending_reboot "$pending"

df -PT -B1 2>/dev/null | awk 'NR > 1 && $2 !~ /^(tmpfs|devtmpfs|squashfs|overlay|efivarfs|proc|sysfs|nsfs|ramfs)$/ {
    printf "@@inv\tdisk\t%s\t%s\t%s\t%s\n", $7, $3, $5, $2 }'

if command -v dpkg-query >/dev/null 2>&1; then
    dpkg-query -W -f='${db:Status-Abbrev}\t${Package}\t${Version}\t${Maintainer}\n' 2>/dev/null |
        awk -F'\t' '$1 ~ /^ii/ { printf "@@inv\tapp\t%s\t%s\t%s\n", $2, $3, $4 }'
elif command -v rpm >/dev/null 2>&1; then
    rpm -qa --qf '@@inv\tapp\t%{NAME}\t%{VERSION}-%{RELEASE}\t%{VENDOR}\n' 2>/dev/null
fi

printf '@@inv\tdone\n'
exit 0
