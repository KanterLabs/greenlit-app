#!/bin/sh
# Proves Docker-action execution two ways: through the shared job workspace
# (a log file under $GITHUB_WORKSPACE, the way
# `crates/greenlit-runtime/tests/actions_docker.rs` also does) and through
# the GITHUB_OUTPUT command-file protocol reaching a Docker action's sibling
# container (ARCHITECTURE.md's now-closed known-issues entry) -- the
# workflow's later "verify everything" step asserts both.
set -e
echo "docker action got: $INPUT_MESSAGE"
echo "$INPUT_MESSAGE" >> "$GITHUB_WORKSPACE/docker-action.log"
echo "echoed=$INPUT_MESSAGE-docker" >> "$GITHUB_OUTPUT"
