#!/usr/bin/env bash
# Stops the r265 dev daemon and the DSH runtimes of its private home.
exec "$(dirname "${BASH_SOURCE[0]}")/r265-dev-daemon.sh" stop
