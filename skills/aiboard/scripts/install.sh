#!/bin/sh
set -eu

repository="zcourts/aiboard"
install_dir="${AIBOARD_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) platform="linux" ;;
  Darwin) platform="macos" ;;
  *) echo "Unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) architecture="x86_64" ;;
  arm64|aarch64) architecture="aarch64" ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM
asset="aiboard-${platform}-${architecture}.tar.gz"
base="https://github.com/${repository}/releases/latest/download"

curl --fail --location --silent --show-error "$base/$asset" --output "$temporary_dir/$asset"
curl --fail --location --silent --show-error "$base/SHA256SUMS" --output "$temporary_dir/SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1 }' "$temporary_dir/SHA256SUMS")"
if [ -z "$expected" ]; then
  echo "Release checksum does not contain $asset" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$temporary_dir/$asset" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$temporary_dir/$asset" | awk '{print $1}')"
fi
[ "$actual" = "$expected" ] || { echo "Checksum verification failed for $asset" >&2; exit 1; }

tar -xzf "$temporary_dir/$asset" -C "$temporary_dir"
mkdir -p "$install_dir"
install -m 0755 "$temporary_dir/aiboard" "$install_dir/aiboard"
echo "Installed $install_dir/aiboard"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "Add $install_dir to PATH, then restart your agent client." ;;
esac
