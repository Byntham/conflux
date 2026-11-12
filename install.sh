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

install_binary() {
  curl -fsSL -o "$1/conflux.download" \
    "https://github.com/$repo/releases/latest/download/conflux-$2"
  chmod +x "$1/conflux.download"
  mv "$1/conflux.download" "$1/conflux"
  echo "Installed conflux to $1/conflux."
}

install_app() {
  apps="$HOME/Applications"
  mkdir -p "$apps"
  work=$(mktemp -d "$apps/.conflux-install.XXXXXX")
  trap 'rm -rf "$work"' EXIT
  curl -fsSL -o "$work/app.zip" \
    "https://github.com/$repo/releases/latest/download/conflux-darwin-aarch64-app.zip" || return 1
  ditto -x -k "$work/app.zip" "$work" || return 1
  [ -d "$work/Conflux.app" ] || return 1
  ln -sf "$apps/Conflux.app/Contents/Helpers/conflux" "$1/conflux" || return 1
  if [ -e "$apps/Conflux.app" ]; then
    mv "$apps/Conflux.app" "$work/previous.app" || return 1
  fi
  if ! mv "$work/Conflux.app" "$apps/Conflux.app"; then
    [ -e "$work/previous.app" ] && mv "$work/previous.app" "$apps/Conflux.app"
    return 1
  fi
  echo "Installed Conflux.app to $apps and linked $1/conflux to it."
}

main() {
  app=yes
  hub=
  for arg in "$@"; do
    case "$arg" in
      --no-app) app=no ;;
      *) hub=$arg ;;
    esac
  done

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
  if [ "$os" != darwin ] || [ "$app" = no ]; then
    install_binary "$bin_dir" "$os-$arch"
  elif ! install_app "$bin_dir"; then
    if [ -e "$HOME/Applications/Conflux.app" ]; then
      echo "Could not update Conflux.app. Try again later." >&2
      exit 1
    fi
    echo "Could not install Conflux.app. Installing the conflux command only."
    install_binary "$bin_dir" "$os-$arch"
  fi
  add_to_path "$bin_dir"

  if [ -n "$hub" ]; then
    "$bin_dir/conflux" join "$hub" </dev/null
  else
    "$bin_dir/conflux" wait </dev/null
  fi
}

main "$@"
