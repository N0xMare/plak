# Sourced by scripts/musl-native.sh and tools/release/package-release.sh:
# checks the native Linux x86_64 musl prerequisites and exports the compiler,
# linker and CARGO_BUILD_TARGET for x86_64-unknown-linux-musl.
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo "native musl build requires a Linux x86_64 host" >&2
  exit 2
fi
if ! command -v rustup >/dev/null; then
  echo "native musl build requires rustup and target x86_64-unknown-linux-musl" >&2
  exit 2
fi
installed_targets="$(rustup target list --installed)"
if ! grep -qx x86_64-unknown-linux-musl <<<"$installed_targets"; then
  echo "missing Rust musl target: rustup target add x86_64-unknown-linux-musl" >&2
  exit 2
fi
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl-musl-gcc}"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER-rust-lld}"
for name in CC_x86_64_unknown_linux_musl CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER; do
  # Rustc adds its own linker-tools directory to PATH when resolving rust-lld.
  if [[ "$name" == CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER && "${!name}" == rust-lld ]]; then
    continue
  fi
  if ! command -v "${!name}" >/dev/null; then
    echo "missing musl compiler/linker: $name=${!name} (set $name to an executable)" >&2
    exit 2
  fi
done
# Nix musl-gcc injects a dynamic loader for static PIE; rust-lld links it natively.
export CARGO_BUILD_TARGET=x86_64-unknown-linux-musl
