# Greenlit convergent base image.
#
# One Dockerfile for every supported Ubuntu runner label; the concrete upstream
# base is chosen at build time via the `BASE_IMAGE` build arg (`ubuntu:24.04`
# for `ubuntu-latest`/`ubuntu-24.04`, `ubuntu:22.04` for `ubuntu-22.04` —
# `greenlit-v0-spec.md` "Stable Linux x64 runners"). `ARG` before `FROM` is the
# documented Docker mechanism for parameterising the base
# (https://docs.docker.com/reference/dockerfile/#understand-how-arg-and-from-interact).
#
# This image is built through the Docker Engine API on first use and tagged
# `greenlit/base:ubuntu-<version>-<content-hash>`; it is never assembled by
# shelling out to `docker` (`AGENTS.md` "Tech"). The build context is assembled
# in memory by `greenlit-runtime`; the only file copied in besides this
# Dockerfile is the private `greenlit-init` helper, which is extracted into the
# build context and never installed as a host command.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# Ownership marker. `litci clean` finds Greenlit's own images by this label
# rather than by matching `greenlit/` in tag text, which any image could be
# tagged with. A per-repo convergent image is produced by committing a
# container started from this one, and `docker commit` carries the config
# (labels included) forward — so converged images inherit the marker and are
# reclaimable by the same query, without `CommitSpec` having to restate it.
LABEL greenlit.managed="1"

# The slim tool set every GitHub Ubuntu runner is expected to carry: shells,
# git/curl/wget for checkout and downloads, jq/tar/unzip for common script
# steps, build-essential for native compiles, ca-certificates for TLS. `sh`
# (dash) ships in the base image already; `bash` is installed explicitly because
# ordinary Ubuntu jobs default to `bash -e {0}`. `--no-install-recommends` and
# the apt-list cleanup keep the convergent base slim.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        git \
        curl \
        wget \
        jq \
        tar \
        unzip \
        build-essential \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The private overlay-init helper. It is the container entrypoint: at container
# start it stacks a throwaway writable overlay over the read-only repo bind and
# execs the job command against the merged view (`greenlit-v0-spec.md` "Security
# model"). Placed under /greenlit/bin so it never shadows a repo tool on PATH.
COPY greenlit-init /greenlit/bin/greenlit-init
RUN chmod 0755 /greenlit/bin/greenlit-init

ENTRYPOINT ["/greenlit/bin/greenlit-init"]
