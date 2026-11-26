#!/usr/bin/env bash
# Every install package ships the remote server programs for both Linux architectures: Winc uploads
# the matching one to the host on connection. A local packaging run may not have built them yet
# (`cargo xtask server`); build them here so the package is never missing them.
#
# Source this file and call `ensure_remote_servers <workspace root>` before copying them.

ensure_remote_servers() {
  local root="$1"
  local name missing=0
  for name in winc-server-Linux-x86_64 winc-server-Linux-aarch64; do
    [[ -f "$root/target/remote-server/$name" ]] || missing=1
  done
  if ((missing)); then
    echo "building the remote server programs (cargo xtask server)" >&2
    (cd "$root" && cargo xtask server)
  fi
}
