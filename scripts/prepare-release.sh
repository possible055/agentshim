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
core_manifest_path=$repository/crates/core/Cargo.toml
napi_manifest_path=$repository/crates/napi/Cargo.toml
lock_path=$repository/Cargo.lock

dsh_package_path=$repository/adapters/dsh/package.json
platform_package_paths="
$repository/adapters/dsh/npm/darwin-arm64/package.json
$repository/adapters/dsh/npm/linux-arm64-gnu/package.json
$repository/adapters/dsh/npm/linux-x64-gnu/package.json
$repository/adapters/dsh/npm/win32-x64-msvc/package.json
"

if [ "$allow_dirty" -eq 0 ]; then
    status=$(git -C "$repository" status --porcelain --untracked-files=all)
    if [ -n "$status" ]; then
        echo "the Git worktree is dirty; pass --allow-dirty explicitly to continue" >&2
        exit 1
    fi
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
            print compare()
        }
    '
}

if ! semver_compare "$version" "$version" >/dev/null; then
    echo "VERSION must be a valid SemVer value" >&2
    exit 1
fi

if ! remote_tags=$(git -C "$repository" ls-remote --refs --tags origin 'refs/tags/v*'); then
    echo "could not inspect release tags on origin" >&2
    exit 1
fi

highest_released_version=""
for tag_ref in $(printf '%s\n' "$remote_tags" | sed -n 's/^[^[:space:]]*[[:space:]]refs\/tags\/v//p'); do
    if ! comparison=$(semver_compare "$tag_ref" "$tag_ref"); then
        continue
    fi
    if [ -z "$highest_released_version" ]; then
        highest_released_version=$tag_ref
        continue
    fi
    comparison=$(semver_compare "$tag_ref" "$highest_released_version")
    if [ "$comparison" -gt 0 ]; then
        highest_released_version=$tag_ref
    fi
done

if [ -n "$highest_released_version" ]; then
    comparison=$(semver_compare "$version" "$highest_released_version")
    if [ "$comparison" -le 0 ]; then
        echo "version $version must be greater than the highest release tag v$highest_released_version on origin" >&2
        exit 1
    fi
fi

lock_before=""
if [ -f "$lock_path" ]; then
    lock_before=$(cat "$lock_path")
fi

update_cargo_manifest() {
    target_manifest=$1
    target_version=$2
    tmp_manifest=$(mktemp)
    awk -v version="$target_version" '
        !changed && $0 ~ /^version[[:space:]]*=[[:space:]]*"[^"]+"[[:space:]]*$/ {
            sub(/"[^"]+"/, "\"" version "\"")
            changed=1
        }
        $0 ~ /^agentshim-core[[:space:]]*=/ && $0 ~ /version[[:space:]]*=[[:space:]]*"[^"]+"/ {
            sub(/version[[:space:]]*=[[:space:]]*"[^"]+"/, "version = \"" version "\"")
        }
        { print }
        END { if (!changed) exit 1 }
    ' "$target_manifest" > "$tmp_manifest" || {
        rm -f "$tmp_manifest"
        echo "could not update version in $target_manifest" >&2
        exit 1
    }
    mv "$tmp_manifest" "$target_manifest"
}

update_json_package() {
    target_pkg=$1
    target_version=$2
    tmp_pkg=$(mktemp)
    sed -E \
        -e "s/^([[:space:]]*\"version\"[[:space:]]*:[[:space:]]*)\"[^\"]+\"/\1\"$target_version\"/" \
        -e "s/(\"dsh-agentshim-[^\"]+\"[[:space:]]*:[[:space:]]*)\"workspace:[^\"]+\"/\1\"workspace:$target_version\"/" \
        "$target_pkg" > "$tmp_pkg"
    mv "$tmp_pkg" "$target_pkg"
}

# Update all Cargo manifests
update_cargo_manifest "$manifest_path" "$version"
update_cargo_manifest "$core_manifest_path" "$version"
update_cargo_manifest "$napi_manifest_path" "$version"

# Update DSH package manifests
update_json_package "$dsh_package_path" "$version"
for path in $platform_package_paths; do
    [ -n "$path" ] && update_json_package "$path" "$version"
done

# Sync Cargo workspace lockfile
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

# Sync pnpm workspace lockfile
if ! (cd "$repository/adapters/dsh" && pnpm install); then
    echo "pnpm install in adapters/dsh failed" >&2
    exit 1
fi

# Validate Cargo crate versions
metadata=$(cargo metadata --locked --no-deps --format-version 1) || {
    echo "cargo metadata --locked failed" >&2
    exit 1
}
for crate in agentshim agentshim-core agentshim-napi; do
    crate_version=$(printf '%s\n' "$metadata" | sed -n "s/.*\"name\"[[:space:]]*:[[:space:]]*\"$crate\"[[:space:]]*,[[:space:]]*\"version\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | sed -n '1p')
    if [ "$crate_version" != "$version" ]; then
        echo "Cargo metadata reports $crate version $crate_version; expected $version" >&2
        exit 1
    fi
done

# Validate DSH package versions
for json_file in "$dsh_package_path" $platform_package_paths; do
    if [ -n "$json_file" ]; then
        pkg_version=$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$json_file" | sed -n '1p')
        if [ "$pkg_version" != "$version" ]; then
            echo "$json_file reports version $pkg_version; expected $version" >&2
            exit 1
        fi
    fi
done

echo "Prepared agentshim and dsh-agentshim version $version."
