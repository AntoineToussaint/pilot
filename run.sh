#!/bin/bash
# Run pilot with libghostty-vt backend
DYLD_LIBRARY_PATH="$(find target/debug/build/libghostty-vt-sys-*/out/ghostty-install/lib -maxdepth 0 2>/dev/null | head -1)" \
  cargo run -p pilot "$@"
