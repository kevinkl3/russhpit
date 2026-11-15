#!/bin/sh

set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

run_image() {
    name=$1
    base_image=$2
    package_family=$3
    image="russhpit-libc-test:${name}"
    registry_volume="russhpit-cargo-registry-${name}"
    target_dir="${repo_dir}/target/libc-${name}"

    echo "==> Build ${base_image}"
    docker build \
        --file "${repo_dir}/docker/Dockerfile" \
        --build-arg "BASE_IMAGE=${base_image}" \
        --build-arg "PACKAGE_FAMILY=${package_family}" \
        --tag "${image}" \
        "${repo_dir}"

    mkdir -p "${target_dir}"

    echo "==> Test ${base_image}"
    docker run --rm \
        --volume "${repo_dir}:/src:ro" \
        --volume "${target_dir}:/target" \
        --volume "${registry_volume}:/root/.cargo/registry" \
        --env CARGO_TARGET_DIR=/target \
        "${image}"
}

run_image debian-12 debian:12 apt
run_image fedora-42 fedora:42 dnf
run_image alpine-3.22 alpine:3.22 apk
