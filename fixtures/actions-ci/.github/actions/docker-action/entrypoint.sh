#!/bin/sh
# This implementation does not wire the GITHUB_ENV/GITHUB_OUTPUT/GITHUB_PATH/
# GITHUB_STEP_SUMMARY command-file protocol into a Docker action's sibling
# container (see ARCHITECTURE.md known-issues log), so this fixture proves
# Docker-action execution the way `crates/greenlit-runtime/tests/actions_docker.rs`
# already does: through the shared job workspace, not $GITHUB_OUTPUT.
set -e
echo "docker action got: $INPUT_MESSAGE"
echo "$INPUT_MESSAGE" >> "$GITHUB_WORKSPACE/docker-action.log"
