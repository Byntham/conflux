#!/bin/sh
set -eu

repo="Byntham/conflux"

add_to_path() {
  case ":$PATH:" in
    *":$1:"*) return ;;
  esac
  case "$(basename "${SHELL:-sh}")" in
    zsh) profile="$HOME/.zshrc" ;;
    bash) profile="$HOME/.bashrc" ;;
    *) profile="$HOME/.profile" ;;
  esac
  line="export PATH=\"$1:\$PATH\""
  if ! grep -qsF "$line" "$profile"; then
    printf '\n%s\n' "$line" >> "$profile"
    echo "Added $1 to your PATH in $profile. Open a new terminal to use conflux."
  fi
}

main() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    amd64) arch=x86_64 ;;
    arm64) arch=aarch64 ;;
  esac
  case "$os-$arch" in
    linux-x86_64 | linux-aarch64 | darwin-aarch64) ;;
    *)
      echo "conflux has no prebuilt binary for $os-$arch." >&2
      echo "Build it with Rust instead: cargo install --git https://github.com/$repo" >&2
      exit 1
      ;;
  esac

  bin_dir="${CONFLUX_BIN_DIR:-$HOME/.local/bin}"
  mkdir -p "$bin_dir"
  curl -fsSL -o "$bin_dir/conflux.download" \
    "https://github.com/$repo/releases/latest/download/conflux-$os-$arch"
  chmod +x "$bin_dir/conflux.download"
  mv "$bin_dir/conflux.download" "$bin_dir/conflux"
  echo "Installed conflux to $bin_dir/conflux."
  add_to_path "$bin_dir"

  if [ "$#" -gt 0 ]; then
    "$bin_dir/conflux" join "$1" </dev/null
  else
    "$bin_dir/conflux" wait </dev/null
  fi
}

main "$@"
