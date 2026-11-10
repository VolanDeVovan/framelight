#!/usr/bin/env bash
# Runs the encoder/capture self-tests against the kernel's vicodec inside a
# throwaway NixOS VM (KVM, lavapipe for Vulkan). No host changes needed.
#   tools/vicodec-vm/run-vm.sh   -> prints the guest log, exits with its status
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
share=$(mktemp -d)
cargo build --release -p framelight --manifest-path "$root/Cargo.toml"
cp "$root/target/release/framelight" "$here/run.sh" "$share/"
SHARE_DIR=$share nix build --impure -o "$share/vm" --expr "
  let nixpkgs = builtins.getFlake \"nixpkgs\";
  in (nixpkgs.lib.nixosSystem {
    system = \"x86_64-linux\";
    modules = [ (import $here/vm.nix { shareDir = builtins.getEnv \"SHARE_DIR\"; }) ];
  }).config.system.build.vm"
(cd "$share" && timeout 600 ./vm/bin/run-nixos-vm >"$share/console.log" 2>&1)
sed 's/\x1b\[[0-9;]*m//g' "$share/result.log"
! grep -q "selftest: FAIL" "$share/result.log"
