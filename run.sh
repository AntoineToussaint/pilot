#!/bin/bash
# Build and run pilot

case "${1:-}" in
  clean)
    echo "Killing daemon and cleaning socket..."
    pkill -f "pilot daemon" 2>/dev/null
    rm -f ~/.pilot/daemon.sock
    shift
    echo "Starting fresh..."
    cargo run -p pilot "$@"
    ;;
  *)
    cargo run -p pilot "$@"
    ;;
esac
