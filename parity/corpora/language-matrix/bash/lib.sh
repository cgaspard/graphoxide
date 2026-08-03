#!/usr/bin/env bash

process() {
  printf '%s\n' "$1"
}

worker_run() {
  process "$1"
}
