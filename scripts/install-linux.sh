#!/bin/bash
# Install MouseTail for the current user on Linux (Wayland: Hyprland / Omarchy, GNOME…). No sudo.
#
#   ./install.sh                  from a release download (uses the included binary)
#   scripts/install-linux.sh      from a source checkout (builds it; needs Rust)
#
# Installs ~/.local/bin/mousetail, runs it as a systemd user service that starts with your
# desktop, and on Omarchy or GNOME adds a status icon to the bar.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin_dir=$HOME/.local/bin
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
unit_dir=$config_home/systemd/user
plugin_id=nz.galengreen.mousetail
omarchy=$config_home/omarchy
gnome_uuid=mousetail@galen.green
gnome_extensions=${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions
# Helper scripts (enable-input, enable-firewall, enable-wake, uninstall) live here, since a `curl | sh` install
# deletes its download when it's done.
share=${XDG_DATA_HOME:-$HOME/.local/share}/mousetail

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }

# GNOME: the desktop says so, or (run from SSH or a TTY) its shell is running for this user.
on_gnome() {
  [[ ${XDG_CURRENT_DESKTOP:-} == *GNOME* ]] || pgrep -u "$(id -u)" -x gnome-shell >/dev/null 2>&1
}

if [[ -x $here/mousetail ]]; then
  # Release download: everything is alongside this script.
  binary=$here/mousetail
  plugin_src=$here/omarchy-plugin/$plugin_id
  gnome_src=$here/gnome-extension/$gnome_uuid
  helpers=$here
  helper_suffix=.sh
else
  repo=$(cd "$here/.." && pwd)
  [[ -f $repo/Cargo.toml ]] || { echo "Can't find MouseTail to install."; exit 1; }
  cargo=$(command -v cargo || true)
  [[ -z $cargo && -x $HOME/.cargo/bin/cargo ]] && cargo=$HOME/.cargo/bin/cargo
  if [[ -z $cargo ]]; then
    echo "Building MouseTail needs Rust (https://rustup.rs), or download a release instead."
    exit 1
  fi
  say "Building MouseTail"
  (cd "$repo" && "$cargo" build --release --quiet -p mousetail)
  binary=$repo/target/release/mousetail
  plugin_src=$repo/integrations/omarchy/$plugin_id
  gnome_src=$repo/integrations/gnome/$gnome_uuid
  helpers=$repo/scripts
  helper_suffix=-linux.sh
fi

install -Dm755 "$binary" "$bin_dir/mousetail"
missing=$(ldd "$bin_dir/mousetail" 2>/dev/null | awk '/not found/ {print $1}' | paste -sd' ' || true)
if [[ -n $missing ]]; then
  echo "MouseTail needs these libraries, which this system is missing: $missing"
  echo "(They come with PipeWire and Opus; install those with your package manager.)"
  exit 1
fi
mkdir -p "$share"
for helper in enable-input enable-firewall enable-wake uninstall; do
  install -m755 "$helpers/$helper$helper_suffix" "$share/$helper.sh"
done

# MouseTail used to be called Kiore: stop and remove the old install (its settings and
# pairings are moved across the first time MouseTail runs).
if [[ -f $unit_dir/kiore.service || -e $HOME/.local/bin/kiore ]]; then
  say "Removing the old Kiore install"
  systemctl --user disable --now kiore.service 2>/dev/null || true
  rm -f "$unit_dir/kiore.service" "$HOME/.local/bin/kiore"
  if [[ -d $omarchy ]]; then
    rm -rf "$omarchy/plugins/nz.galengreen.kiore"
    if [[ -f $omarchy/shell.json ]] && command -v jq >/dev/null; then
      jq '.bar.layout |= (if . == null then . else map_values(map(select(.id != "nz.galengreen.kiore"))) end)' \
        "$omarchy/shell.json" > "$omarchy/shell.json.tmp" && mv "$omarchy/shell.json.tmp" "$omarchy/shell.json"
    fi
  fi
fi

say "Starting it with your desktop"
mkdir -p "$unit_dir"
cat > "$unit_dir/mousetail.service" <<UNIT
[Unit]
Description=MouseTail keyboard, mouse and clipboard sharing
After=graphical-session.target
PartOf=graphical-session.target
# Keep retrying however often it fails early on (e.g. while the desktop is still starting).
StartLimitIntervalSec=0

[Service]
ExecStart=%h/.local/bin/mousetail run
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
UNIT
systemctl --user daemon-reload
systemctl --user enable --quiet mousetail.service
systemctl --user restart mousetail.service
# Some desktops (Hyprland or Sway started without uwsm, say) never tell systemd the session
# has started, so nothing started "with your desktop" would run at the next login.
if ! systemctl --user is-active --quiet graphical-session.target; then
  echo "    Your desktop doesn't start systemd's graphical session, so start MouseTail from its"
  echo "    config instead. Hyprland: exec-once = systemctl --user start mousetail"
  echo "    Sway: exec systemctl --user start mousetail"
fi

if [[ -d $omarchy ]]; then
  say "Adding MouseTail to the Omarchy bar"
  rm -rf "$omarchy/plugins/$plugin_id"
  mkdir -p "$omarchy/plugins"
  cp -r "$plugin_src" "$omarchy/plugins/$plugin_id"
  shell=$omarchy/shell.json
  if [[ -f $shell ]] && command -v jq >/dev/null; then
    if jq -e --arg id "$plugin_id" '[.bar.layout[]?[]?.id] | index($id)' "$shell" >/dev/null; then
      :
    elif jq -e '.bar.layout.right | type == "array"' "$shell" >/dev/null; then
      cp "$shell" "$shell.before-mousetail"
      jq --arg id "$plugin_id" '.bar.layout.right = [{"id": $id}] + .bar.layout.right' \
        "$shell" > "$shell.tmp" && mv "$shell.tmp" "$shell"
    else
      echo "    Your bar uses Omarchy's default layout; add \"MouseTail\" from the bar settings."
    fi
  fi
fi

# GNOME only runs an extension made for its version.
gnome_version=$(gnome-shell --version 2>/dev/null | grep -oE '[0-9]+' | head -1 || true)
gnome_supported=$(grep -so '"shell-version": *\[[^]]*' "$gnome_src/metadata.json" | grep -oE '[0-9]+' | paste -sd/ || true)
if on_gnome && [[ -n $gnome_version && -n $gnome_supported && /$gnome_supported/ != */$gnome_version/* ]]; then
  echo
  echo "MouseTail's top-bar menu needs GNOME $gnome_supported (this is GNOME $gnome_version), so it isn't added."
elif on_gnome && [[ -d $gnome_src ]]; then
  say "Adding MouseTail to GNOME's top bar"
  rm -rf "${gnome_extensions:?}/$gnome_uuid"
  mkdir -p "$gnome_extensions"
  cp -r "$gnome_src" "$gnome_extensions/$gnome_uuid"
  # GNOME only finds a new extension when you log in (on Wayland it can't reload), so if it
  # won't switch this one on now, put it on the list for the next login.
  if ! gnome-extensions enable "$gnome_uuid" 2>/dev/null; then
    gjs -c "
      const {Gio} = imports.gi;
      const shell = new Gio.Settings({schema_id: 'org.gnome.shell'});
      const on = shell.get_strv('enabled-extensions');
      if (!on.includes('$gnome_uuid'))
        shell.set_strv('enabled-extensions', [...on, '$gnome_uuid']);
      shell.set_strv('disabled-extensions', shell.get_strv('disabled-extensions').filter(u => u !== '$gnome_uuid'));
      Gio.Settings.sync();" 2>/dev/null || true
    echo "    It shows in the top bar after you next log in."
  fi
  if [[ $(gsettings get org.gnome.shell disable-user-extensions 2>/dev/null) == true ]]; then
    echo "    GNOME's extensions are switched off: turn them on in the Extensions app to see it."
  fi
fi

# On desktops without Wayland's virtual-input protocols, being controlled needs uinput access.
sleep 3
if ! "$bin_dir/mousetail" status 2>/dev/null | grep -q "can be controlled"; then
  echo
  echo "To let other computers control this one on this desktop, run once (asks for your password):"
  echo "    $share/enable-input.sh"
fi

# A firewall that drops what it hasn't been told about (Omarchy and Fedora turn one on) stops
# other computers reaching this one. Offer to open MouseTail's port; sudo asks first.
if "$bin_dir/mousetail" status 2>/dev/null | grep -q "the firewall"; then
  echo
  echo "This computer's firewall stops other computers reaching MouseTail."
  if [[ -t 1 ]] && { : </dev/tty; } 2>/dev/null; then
    say "Opening MouseTail's port to your local network (asks for your password)"
    "$share/enable-firewall.sh" </dev/tty || {
      echo "    That didn't work. To try again later: $share/enable-firewall.sh"
    }
  else
    echo "To fix it, run once (asks for your password):"
    echo "    $share/enable-firewall.sh"
  fi
fi

if systemctl --user is-active --quiet lan-mouse.service 2>/dev/null; then
  echo
  echo "Note: Lan Mouse is also running. Use one or the other:"
  echo "    systemctl --user disable --now lan-mouse.service"
fi

echo
say "Done. To connect another computer, click Pair… in MouseTail there, or run: mousetail pair"
echo "    To let your Mac wake this computer from sleep: $share/enable-wake.sh"
echo "    To remove MouseTail: $share/uninstall.sh"
