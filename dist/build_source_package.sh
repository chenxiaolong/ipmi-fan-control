#!/bin/bash

# Generates OS-specific packaging with metadata fields filled in.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

# Parse version
# - version: Base tag name
# - plus_rev: Number of commits since tag (0 if building tag)
# - git_commit: git short commit ID of HEAD
# - full_version: ${version}.r${plus_rev}.git${git_commit}
compute_version() {
    local raw_version
    local components

    if [[ -n "${VERSION_OVERRIDE:-}" ]]; then
        raw_version=${VERSION_OVERRIDE}
    else
        raw_version=$(git describe --long)
    fi

    IFS='-' read -r -a components <<< "${raw_version}"

    version=${components[0]#v}
    plus_rev=${components[1]:-}
    git_commit=${components[2]:-}
    git_commit=${git_commit#g}

    full_version=${version}
    if [[ -n "${plus_rev}" ]]; then
        full_version+=.r${plus_rev}
    fi
    if [[ -n "${git_commit}" ]]; then
        full_version+=.git${git_commit}
    fi
}

check_tools() {
    local cmd missing=()

    for cmd in "${@}"; do
        if ! command -v "${cmd}" >/dev/null; then
            missing+=("${cmd}")
        fi
    done

    if [[ "${#missing[@]}" -gt 0 ]]; then
        echo >&2 "The following tools must be installed:"
        for cmd in "${missing[@]}"; do
            echo >&2 "- ${cmd}"
        done
        exit 1
    fi
}

# Build tarball to be used for all distro's packaging
build_tarball() {
    local prefix="ipmi-fan-control-${full_version}"
    tarball="${output_dir}/${prefix}.vendored.tar.xz"

    local staging_dir="${temp_dir}/tarball/${prefix}"
    mkdir -p "${staging_dir}"

    git -C "$(git rev-parse --show-toplevel)" \
        archive \
        --format tar \
        HEAD \
        | tar -C "${staging_dir}" -xf -

    # Include all dependencies into the tarball because build services like
    # launchpad.net and build.opensuse.org do not allow internet access during
    # builds.
    pushd "${staging_dir}" >/dev/null
    cargo vendor
    # Remove prebuilt winapi libraries
    rm -r vendor/winapi-*/lib
    popd >/dev/null

    mkdir "${staging_dir}"/.cargo
    cp cargo.vendored.toml "${staging_dir}"/.cargo/config

    tar -C "${temp_dir}/tarball" -Jcvf "${tarball}" "${prefix}"
}

# Build source RPM for Fedora/CentOS
build_srpm() {
    check_tools rpmbuild

    mkdir -p "${temp_dir}"/rpm/{SOURCES,SPECS}
    sed \
        -e "s/@VERSION@/${version}/g" \
        -e "s/@PLUS_REV@/${plus_rev}/g" \
        -e "s/@GIT_COMMIT@/${git_commit}/g" \
        -e "s/@TARBALL_NAME@/$(basename "${tarball}")/g" \
        < rpm/ipmi-fan-control.spec.in \
        > "${temp_dir}"/rpm/SPECS/ipmi-fan-control.spec
    cp ipmi-fan-control.service.in "${temp_dir}"/rpm/SOURCES/
    cp "${tarball}" "${temp_dir}"/rpm/SOURCES/

    rpmbuild \
        --define "_topdir ${temp_dir}/rpm" \
        -bs "${temp_dir}"/rpm/SPECS/ipmi-fan-control.spec

    mkdir -p "${output_dir}"/rpm
    cp -v "${temp_dir}"/rpm/SRPMS/*.src.rpm "${output_dir}"/rpm/
}

build_pkgbuild() {
    check_tools updpkgsums

    mkdir -p "${temp_dir}"/pkgbuild
    sed \
        -e "s/@VERSION@/${full_version}/g" \
        -e "s/@TARBALL_NAME@/$(basename "${tarball}")/g" \
        < pkgbuild/PKGBUILD.in \
        > "${temp_dir}"/pkgbuild/PKGBUILD

    cp "${tarball}" "${temp_dir}"/pkgbuild/

    updpkgsums "${temp_dir}/pkgbuild/PKGBUILD"

    mkdir -p "${output_dir}"/pkgbuild
    cp -v "${temp_dir}"/pkgbuild/* "${output_dir}"/pkgbuild/
}

# Build deb source package for Debian/Ubuntu
build_dsc() {
    check_tools dch debuild
    # These are here to make the build fail faster. Building a source package
    # requires all build deps to be installed because the process runs
    # `debian/rules clean`.
    check_tools cargo dh-exec

    cp "${tarball}" "${temp_dir}/ipmi-fan-control_${full_version}.orig.tar.xz"
    tar -xf "${tarball}" -C "${temp_dir}"

    local source_dir="${temp_dir}/ipmi-fan-control-${full_version}"

    cp -r debian "${source_dir}"/
    cp ipmi-fan-control.service.in "${source_dir}"/debian/

    pushd "${temp_dir}/ipmi-fan-control-${full_version}" >/dev/null

    local -a dch_extra_args=() debuild_extra_args=()

    # The target distro and version suffix might be set to make the source
    # package uploadable.
    if [[ -n "${dsc_distro}" ]]; then
        dch_extra_args+=(-D "${dsc_distro}")
    fi

    if [[ "${dsc_signed}" != true ]]; then
        dch_extra_args+=(-M)
        debuild_extra_args+=(-us -uc)
    fi

    # Create dummy changelog
    dch \
        --create \
        --package ipmi-fan-control \
        -v "${full_version}-1${dsc_suffix}" \
        "${dch_extra_args[@]}" \
        "Automatically built from version ${full_version}"

    debuild -S "${debuild_extra_args[@]}"

    popd >/dev/null

    mkdir -p "${output_dir}"/debian
    find "${temp_dir}" \
        -mindepth 1 \
        -maxdepth 1 \
        -type f \
        -exec cp -t "${output_dir}"/debian/ '{}' '+'
}

clean_up() {
    if [[ "${keep_temp_dir}" == true ]]; then
        echo >&2 "Skipping deletion of temp directory: ${temp_dir}"
    else
        rm -r "${temp_dir}"
    fi
}

help() {
    echo "Usage: ${0} -t <target> [<option>...]"
    echo
    echo 'Options:'
    echo '  -t, --target         Type of source package to build'
    echo '  -k, --keep-temp-dir  Do not delete temp directory on exit'
    echo
    echo 'dsc-only options:'
    echo '  --dsc-distro NAME    Target distro for dsc package upload'
    echo '  --dsc-suffix SUFFIX  Version suffix for dsc package upload'
    echo '  --dsc-signed         Create signed dsc/changes files'
    echo
    echo 'Valid targets:'
    echo '  tarball  - Build a source tarball using "git archive"'
    echo '  srpm     - Build an SRPM'
    echo '  pkgbuild - Build a PKGBUILD'
    echo '  dsc      - Build a deb source package'
    echo
    echo 'Debian-specific notes:'
    echo '  The --dsc-distro and --dsc-suffix options are only useful when'
    echo '  building Debian source packages that are meant to be uploaded to'
    echo '  eg. launchpad.net where the same repo is used for multiple distro'
    echo '  versions. --dsc-distro specifies the target distro name (eg. "focal")'
    echo '  and --dsc-suffix specifies a version suffix (eg. "~ubuntu20.04") that'
    echo '  makes the version number unique.'
    echo
    echo '  If --dsc-signed is specified, then the resulting .dsc and .changes'
    echo '  files will be signed. This requires DEBFULLNAME and DEBEMAIL to'
    echo '  match the signing GPG key.'
}

in_array() {
    local item needle=${1}
    shift 1

    for item in "${@}"; do
        if [[ "${item}" == "${needle}" ]]; then
            return 0
        fi
    done

    return 1
}

parse_args() {
    local args target=
    if ! args=$(getopt -o hkt: -l dsc-distro:,dsc-signed,dsc-suffix:,help,keep-temp-dir,target: -n "${0}" -- "${@}"); then
        echo >&2 'Failed to parse arguments'
        help >&2
        exit 1
    fi

    eval set -- "${args}"

    keep_temp_dir=false
    dsc_signed=false
    dsc_distro=
    dsc_suffix=

    while true; do
        case "${1}" in
        -h|--help)
            help
            exit
            ;;
        -k|--keep-temp-dir)
            keep_temp_dir=true
            shift 1
            ;;
        -t|--target)
            target=${2}
            shift 2
            ;;
        --dsc-distro)
            dsc_distro=${2}
            shift 2
            ;;
        --dsc-signed)
            dsc_signed=true
            shift
            ;;
        --dsc-suffix)
            dsc_suffix=${2}
            shift 2
            ;;
        --)
            shift
            break
            ;;
        esac
    done

    if [[ "${#}" -ne 0 ]]; then
        echo >&2 "Unexpected arguments: ${*}"
        help >&2
        exit 1
    fi

    actions=()

    case "${target}" in
    tarball)
        actions+=(tarball)
        ;;
    srpm)
        actions+=(tarball srpm)
        ;;
    pkgbuild)
        actions+=(tarball pkgbuild)
        ;;
    dsc)
        actions+=(tarball dsc)
        ;;
    '')
        echo >&2 "No target specified"
        help >&2
        exit 1
        ;;
    *)
        echo >&2 "Unknown target: ${target}"
        help >&2
        exit 1
        ;;
    esac

    if ! in_array dsc "${actions[@]}" \
            && [[ -n "${dsc_distro}" || -n "${dsc_suffix}" || "${dsc_signed}" != false ]]; then
        echo >&2 "--dsc-* options can only used when building a dsc source package."
        exit 1
    fi
}

###

parse_args "${@}"

output_dir=$(pwd)/output
mkdir -p "${output_dir}"

temp_dir=$(mktemp -d -p "$(pwd)")
trap clean_up EXIT

compute_version
echo "Version: ${version}"
echo "Commits since tag: ${plus_rev}"
echo "HEAD short commit: ${git_commit}"

for action in "${actions[@]}"; do
    build_"${action}"
done
