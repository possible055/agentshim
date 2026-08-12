#!/bin/sh
set -eu

allow_dirty=0
version=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --allow-dirty)
            allow_dirty=1
            shift
            ;;
        --)
            shift
            if [ "$#" -ne 1 ]; then
                echo "usage: $0 [--allow-dirty] VERSION" >&2
                exit 2
            fi
            version=$1
            shift
            ;;
        -*)
            echo "unknown option: $1" >&2
            exit 2
            ;;
        *)
            if [ -n "$version" ]; then
                echo "usage: $0 [--allow-dirty] VERSION" >&2
                exit 2
            fi
            version=$1
            shift
            ;;
    esac
done

if [ -z "$version" ]; then
    echo "usage: $0 [--allow-dirty] VERSION" >&2
    exit 2
fi

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
manifest_path=$repository/Cargo.toml
lock_path=$repository/Cargo.lock

if [ "$allow_dirty" -eq 0 ]; then
    status=$(git -C "$repository" status --porcelain --untracked-files=all)
    if [ -n "$status" ]; then
        echo "the Git worktree is dirty; pass --allow-dirty explicitly to continue" >&2
        exit 1
    fi
fi

current_version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$manifest_path" | sed -n '1p')
if [ -z "$current_version" ]; then
    echo "Cargo.toml does not contain a package version" >&2
    exit 1
fi

semver_compare() {
    awk -v current="$1" -v candidate="$2" '
        function parse(value, prefix,    plus, core_pre, dash, core, pre, parts, i, identifiers) {
            if (value !~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$/) return 0
            split(value, plus, /\+/)
            core_pre = plus[1]
            dash = index(core_pre, "-")
            if (dash) {
                core = substr(core_pre, 1, dash - 1)
                pre = substr(core_pre, dash + 1)
            } else {
                core = core_pre
                pre = ""
            }
            parts = split(core, core_parts, /\./)
            if (parts != 3) return 0
            for (i = 1; i <= 3; i++) {
                if (core_parts[i] ~ /^0[0-9]+$/) return 0
                if (prefix == "left") left_core[i] = core_parts[i]
                else right_core[i] = core_parts[i]
            }
            if (prefix == "left") left_pre = pre
            else right_pre = pre
            if (pre != "") {
                identifiers = split(pre, pre_parts, /\./)
                for (i = 1; i <= identifiers; i++) {
                    if (pre_parts[i] ~ /^0[0-9]+$/) return 0
                    if (prefix == "left") left_identifiers[i] = pre_parts[i]
                    else right_identifiers[i] = pre_parts[i]
                }
            }
            if (prefix == "left") left_count = (pre == "" ? 0 : identifiers)
            else right_count = (pre == "" ? 0 : identifiers)
            return 1
        }
        function compare(    i, left_is_pre, right_is_pre, count, left_id, right_id, left_num, right_num) {
            for (i = 1; i <= 3; i++) {
                if ((left_core[i] + 0) < (right_core[i] + 0)) return -1
                if ((left_core[i] + 0) > (right_core[i] + 0)) return 1
            }
            left_is_pre = left_pre != ""
            right_is_pre = right_pre != ""
            if (!left_is_pre && !right_is_pre) return 0
            if (!left_is_pre) return 1
            if (!right_is_pre) return -1
            count = left_count < right_count ? left_count : right_count
            for (i = 1; i <= count; i++) {
                left_id = left_identifiers[i]
                right_id = right_identifiers[i]
                left_num = left_id ~ /^[0-9]+$/
                right_num = right_id ~ /^[0-9]+$/
                if (left_num && right_num) {
                    if ((left_id + 0) < (right_id + 0)) return -1
                    if ((left_id + 0) > (right_id + 0)) return 1
                } else if (left_num && !right_num) {
                    return -1
                } else if (!left_num && right_num) {
                    return 1
                } else if (left_id != right_id) {
                    return (left_id < right_id) ? -1 : 1
                }
            }
            if (left_count < right_count) return -1
            if (left_count > right_count) return 1
            return 0
        }
        BEGIN {
            if (!parse(current, "left") || !parse(candidate, "right")) exit 2
            result = compare()
            if (result >= 0) exit 3
            exit 0
        }
    '
}

if semver_compare "$current_version" "$version"; then
    :
else
    result=$?
    if [ "$result" -eq 2 ]; then
        echo "VERSION and the current version must be valid SemVer values" >&2
    else
        echo "version $version must be greater than the current version $current_version" >&2
    fi
    exit 1
fi

lock_before=""
if [ -f "$lock_path" ]; then
    lock_before=$(cat "$lock_path")
fi

tmp_manifest=$(mktemp)
trap 'rm -f "$tmp_manifest"' EXIT HUP INT TERM
awk -v version="$version" '
    !changed && $0 ~ /^version[[:space:]]*=[[:space:]]*"[^"]+"[[:space:]]*$/ {
        sub(/"[^"]+"/, "\"" version "\"")
        changed=1
    }
    { print }
    END { if (!changed) exit 1 }
' "$manifest_path" > "$tmp_manifest" || {
    echo "Cargo.toml does not contain a package version" >&2
    exit 1
}
if ! cmp -s "$manifest_path" "$tmp_manifest"; then
    if ! mv "$tmp_manifest" "$manifest_path"; then
        echo "could not update Cargo.toml" >&2
        exit 1
    fi
else
    rm -f "$tmp_manifest"
fi
trap - EXIT HUP INT TERM

if ! cargo update --manifest-path "$manifest_path" --workspace; then
    echo "cargo update --workspace failed; inspect the working tree" >&2
    exit 1
fi

if [ -n "$lock_before" ]; then
    lock_after=$(cat "$lock_path")
    normalized_before=$(printf '%s\n' "$lock_before" | sed '/^version[[:space:]]*=[[:space:]]*"[^"]*"[[:space:]]*$/d; /^checksum[[:space:]]*=[[:space:]]*"[^"]*"[[:space:]]*$/d' | tr -d '\r')
    normalized_after=$(printf '%s\n' "$lock_after" | sed '/^version[[:space:]]*=[[:space:]]*"[^"]*"[[:space:]]*$/d; /^checksum[[:space:]]*=[[:space:]]*"[^"]*"[[:space:]]*$/d' | tr -d '\r')
    if [ "$normalized_after" != "$normalized_before" ]; then
        echo "Cargo.lock changed beyond dependency versions and checksums; inspect the working tree" >&2
        exit 1
    fi
fi

metadata=$(cargo metadata --locked --no-deps --format-version 1) || {
    echo "cargo metadata --locked failed" >&2
    exit 1
}
metadata_version=$(printf '%s\n' "$metadata" | sed -n 's/.*"name"[[:space:]]*:[[:space:]]*"codexshim"[[:space:]]*,[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | sed -n '1p')
if [ "$metadata_version" != "$version" ]; then
    echo "Cargo metadata reports version $metadata_version; expected $version" >&2
    exit 1
fi

echo "Prepared codexshim version $version."
