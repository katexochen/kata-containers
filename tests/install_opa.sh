#!/usr/bin/env bash
#
# SPDX-License-Identifier: Apache-2.0

[[ -n "${DEBUG}" ]] && set -o xtrace

test_dir=$(realpath "$(dirname "${BASH_SOURCE[0]}")")
source "${test_dir}/common.bash"

install_opa()
{
    local url
    local version
    url=$(get_test_version "external.opa.url")
    version=$(get_test_version "external.opa.version")

    if opa version | grep -q "${version}"; then
        info "OPA version ${version} is already installed"
        return 0
    fi

    curl -fsSL "${url}/releases/download/${version}/opa_linux_amd64_static" \
        -o "/usr/local/bin/opa" \
        || die "Failed to download OPA binary"

    chmod +x "/usr/local/bin/opa" \
        || die "Failed to make OPA binary executable"

    command -v opa &>/dev/null \
        || die "OPA binary not found in PATH after installation"

    info "Successfully installed OPA version ${version}"
}

install_opa
