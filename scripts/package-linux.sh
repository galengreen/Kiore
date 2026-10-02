#!/bin/bash
# Build the Linux release tarball: dist/mousetail-linux-<arch>.tar.gz
# It holds the binary, install/uninstall scripts, the Omarchy bar plugin and the GNOME
# extension; no Rust needed to install it.
set -euo pipefail
cd "$(dirname "$0")/.."
arch=$(uname -m)
name=mousetail-linux-$arch
out=dist/$name

cargo build --release --locked -p mousetail
rm -rf "$out" && mkdir -p "$out/omarchy-plugin" "$out/gnome-extension"
cp target/release/mousetail "$out/"
strip "$out/mousetail" 2>/dev/null || true
cp scripts/install-linux.sh "$out/install.sh"
cp scripts/uninstall-linux.sh "$out/uninstall.sh"
cp scripts/enable-wake-linux.sh "$out/enable-wake.sh"
cp scripts/enable-input-linux.sh "$out/enable-input.sh"
cp scripts/enable-firewall-linux.sh "$out/enable-firewall.sh"
cp -r integrations/omarchy/nz.galengreen.mousetail "$out/omarchy-plugin/"
cp -r integrations/gnome/mousetail@galen.green "$out/gnome-extension/"
# The plugin's and the extension's versions follow MouseTail's.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
sed -i.bak "s/\"version\": \"[^\"]*\"/\"version\": \"$version\"/" "$out/omarchy-plugin/nz.galengreen.mousetail/manifest.json"
rm "$out/omarchy-plugin/nz.galengreen.mousetail/manifest.json.bak"
sed -i.bak "s/\"version-name\": \"[^\"]*\"/\"version-name\": \"$version\"/" "$out/gnome-extension/mousetail@galen.green/metadata.json"
rm "$out/gnome-extension/mousetail@galen.green/metadata.json.bak"
cp LICENSE "$out/"
cat > "$out/README.txt" <<'TXT'
MouseTail for Linux (Wayland)

  ./install.sh        install for your user (no sudo) and start it with your desktop
  ./uninstall.sh      remove it again
  ./enable-wake.sh    optional: let other computers wake this one from sleep (asks for sudo)
  ./enable-input.sh   only if MouseTail says so (GNOME, KDE): allow it to control this computer
  ./enable-firewall.sh  only if MouseTail says so (Omarchy, Fedora): let other computers reach it

Then pair: click Pair… in MouseTail on the other computer, or run: mousetail pair
MouseTail keeps itself up to date (mousetail set updates off to stop that).
https://github.com/galengreen/mousetail
TXT
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "built dist/$name.tar.gz"
