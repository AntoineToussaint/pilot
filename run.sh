#!/bin/bash
# Build and run pilot

case "${1:-}" in
  clean|kill|reset)
    echo "Killing daemon and cleaning socket..."
    pkill -f "pilot daemon" 2>/dev/null
    rm -f ~/.pilot/daemon.sock
    echo "Done. Run ./run.sh to start fresh."
    ;;
  *)
    # Kill stale daemon so it restarts with latest code.
    pkill -f "pilot daemon" 2>/dev/null
    rm -f ~/.pilot/daemon.sock
    cargo run -p pilot "$@"
    ;;
esac
