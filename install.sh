#!/bin/sh
set -eu
LC_ALL=C
export LC_ALL

nemu_installer_version=''

nemu_fail() {
    printf 'nemu: %s\n' "$*" >&2
    exit 1
}

nemu_download() {
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --connect-timeout 15 --max-time 300 --retry 3 --output "$2" "$1"
}

nemu_cleanup() {
    if [ -n "$nemu_staged" ]; then
        rm -f -- "$nemu_staged"
    fi
    rm -f -- "$nemu_tmp/releases.json" "$nemu_tmp/$nemu_archive" \
        "$nemu_tmp/$nemu_archive.sha256" "$nemu_tmp/nemu"
    rmdir "$nemu_tmp" || :
}

nemu_latest_version() {
    nemu_page=1
    nemu_latest_date='-'
    nemu_latest_tag='-'
    while :; do
        nemu_download \
            "https://api.github.com/repos/terraceio/nemu/releases?per_page=100&page=$nemu_page" \
            "$nemu_tmp/releases.json" || nemu_fail 'Could not fetch release metadata from GitHub.'
        nemu_info=$(/usr/bin/osascript -l JavaScript - "$nemu_tmp/releases.json" <<'JXA'
ObjC.import('Foundation');
function run(args) {
    const text = $.NSString.stringWithContentsOfFileEncodingError(args[0], $.NSUTF8StringEncoding, null);
    const releases = JSON.parse(ObjC.unwrap(text));
    if (!Array.isArray(releases)) throw new Error('Expected a GitHub release list');
    let newest = null;
    for (const release of releases) {
        if (release.draft || release.prerelease ||
            !/^nemu-v[0-9]+\.[0-9]+\.[0-9]+(?:\+[0-9A-Za-z.-]+)?$/.test(release.tag_name)) continue;
        if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(release.published_at))
            throw new Error('Invalid Nemu release publication date');
        if (!newest || release.published_at > newest.published_at) newest = release;
    }
    return releases.length + ' ' + (newest ? newest.published_at + ' ' + newest.tag_name : '- -');
}
JXA
        ) || nemu_fail 'Could not read release metadata.'
        IFS=' ' read -r nemu_count nemu_date nemu_tag <<EOF
$nemu_info
EOF
        if [ "$nemu_tag" != '-' ] && [ "$nemu_date" \> "$nemu_latest_date" ]; then
            nemu_latest_date=$nemu_date
            nemu_latest_tag=$nemu_tag
        fi
        [ "$nemu_count" -eq 100 ] || break
        nemu_page=$((nemu_page + 1))
    done
    [ "$nemu_latest_tag" != '-' ] || nemu_fail 'No stable Nemu release has been published yet.'
    nemu_version=${nemu_latest_tag#nemu-v}
}

nemu_main() {
    [ "$(uname -s)" = Darwin ] || nemu_fail 'This installer supports Apple Silicon macOS only.'
    [ "$(uname -m)" = arm64 ] || nemu_fail 'Run this installer from a native Apple Silicon terminal.'
    for nemu_command in curl tar shasum mktemp install; do
        command -v "$nemu_command" >/dev/null 2>&1 || nemu_fail "Missing command: $nemu_command"
    done
    nemu_archive=nemu-aarch64-apple-darwin.tar.gz
    nemu_staged=''
    nemu_tmp=$(mktemp -d "${TMPDIR:-/tmp}/nemu-install.XXXXXXXX")
    trap nemu_cleanup 0
    trap 'exit 130' INT
    trap 'exit 143' TERM

    nemu_version=${NEMU_VERSION:-$nemu_installer_version}
    if [ -z "$nemu_version" ]; then
        nemu_latest_version
    fi
    case "$nemu_version" in
        ''|[!0-9]*|*[!0-9A-Za-z.+-]*) nemu_fail 'NEMU_VERSION must be a version such as 0.1.0 (without nemu-v).' ;;
    esac
    nemu_url="https://github.com/terraceio/nemu/releases/download/nemu-v$nemu_version"
    printf 'Downloading Nemu %s…\n' "$nemu_version"
    nemu_download "$nemu_url/$nemu_archive" "$nemu_tmp/$nemu_archive" || nemu_fail 'Binary download failed.'
    nemu_download "$nemu_url/$nemu_archive.sha256" "$nemu_tmp/$nemu_archive.sha256" || nemu_fail 'Checksum download failed.'
    IFS=' ' read -r nemu_expected nemu_filename < "$nemu_tmp/$nemu_archive.sha256"
    case "$nemu_expected" in ''|*[!0-9a-fA-F]*) nemu_fail 'Invalid SHA-256 checksum.' ;; esac
    [ "${#nemu_expected}" -eq 64 ] && [ "$nemu_filename" = "$nemu_archive" ] || nemu_fail 'Invalid checksum manifest.'
    nemu_actual=$(shasum -a 256 "$nemu_tmp/$nemu_archive")
    [ "${nemu_actual%% *}" = "$nemu_expected" ] || nemu_fail 'Checksum mismatch; nothing was installed.'
    tar -xzOf "$nemu_tmp/$nemu_archive" nemu > "$nemu_tmp/nemu"
    chmod 755 "$nemu_tmp/nemu"
    [ "$("$nemu_tmp/nemu" --version)" = "nemu $nemu_version" ] || nemu_fail 'Downloaded binary has the wrong version.'

    nemu_dir=${NEMU_INSTALL_DIR:-"$HOME/.local/bin"}
    mkdir -p "$nemu_dir"
    [ ! -d "$nemu_dir/nemu" ] || nemu_fail "$nemu_dir/nemu is a directory."
    nemu_staged=$(mktemp "$nemu_dir/.nemu.XXXXXXXX")
    install -m 755 "$nemu_tmp/nemu" "$nemu_staged"
    mv -f "$nemu_staged" "$nemu_dir/nemu"
    nemu_staged=''
    printf 'Installed Nemu %s to %s/nemu\n' "$nemu_version" "$nemu_dir"
    case ":${PATH:-}:" in
        *":$nemu_dir:"*) ;;
        *) printf 'Add %s to your PATH before running nemu.\n' "$nemu_dir" ;;
    esac
}

nemu_main "$@"
