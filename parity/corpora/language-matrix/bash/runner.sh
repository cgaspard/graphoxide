#!/usr/bin/env bash

source ./lib.sh

main() {
  worker_run "${1:-matrix}"
}

main "$@"
