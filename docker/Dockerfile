ARG BASE_IMAGE=debian:12
FROM ${BASE_IMAGE}

ARG PACKAGE_FAMILY

RUN case "${PACKAGE_FAMILY}" in \
        apt) \
            apt-get update && \
            DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
                build-essential \
                ca-certificates \
                curl \
                openssh-client && \
            rm -rf /var/lib/apt/lists/* \
            ;; \
        dnf) \
            dnf install --assumeyes \
                ca-certificates \
                curl \
                gcc \
                make \
                openssh-clients && \
            dnf clean all \
            ;; \
        apk) \
            apk add --no-cache \
                build-base \
                ca-certificates \
                curl \
                openssh-client \
            ;; \
        *) \
            echo "Unsupported package family: ${PACKAGE_FAMILY}" >&2; \
            exit 1 \
            ;; \
    esac

RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs | \
        sh -s -- -y --profile minimal --default-toolchain stable

ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src

CMD ["sh", "/src/scripts/cargo-test-and-openssh-smoke.sh"]
