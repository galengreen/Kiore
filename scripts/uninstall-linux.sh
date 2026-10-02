#!/bin/bash
# Remove MouseTail from this user account (undoes scripts/install-linux.sh).
#   scripts/uninstall-linux.sh [--forget]    --forget also deletes pairings and identity
set -uo pipefail
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
plugin_id=nz.galengreen.mousetail
omarchy=$config_home/omarchy

systemctl --user disable --now mousetail.service 2>/dev/null
rm -f "$config_home/systemd/user/mousetail.service"
systemctl --user daemon-reload
rm -f "$HOME/.local/bin/mousetail"

if [[ -d $omarchy ]]; then
  rm -rf "$omarchy/plugins/$plugin_id"
  shell=$omarchy/shell.json
  if [[ -f $shell ]] && command -v jq >/dev/null; then
    jq --arg id "$plugin_id" '.bar.layout |= (if . == null then . else map_values(map(select(.id != $id))) end)' \
      "$shell" > "$shell.tmp" && mv "$shell.tmp" "$shell"
  fi
fi

gnome_uuid=mousetail@galen.green
gnome_extension=${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$gnome_uuid
if [[ -d $gnome_extension ]]; then
  gnome-extensions disable "$gnome_uuid" 2>/dev/null || gjs -c "
    const {Gio} = imports.gi;
    const shell = new Gio.Settings({schema_id: 'org.gnome.shell'});
    shell.set_strv('enabled-extensions', shell.get_strv('enabled-extensions').filter(u => u !== '$gnome_uuid'));
    Gio.Settings.sync();" 2>/dev/null
  rm -rf "$gnome_extension"
fi

rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/mousetail"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/mousetail"
[[ ${1:-} == --forget ]] && rm -rf "$config_home/mousetail"
echo "MouseTail removed."
# enable-firewall.sh opened a port (with your password), so closing it needs it too.
nets="10.0.0.0/8 172.16.0.0/12 192.168.0.0/16"
if grep -qs "MouseTail" /etc/ufw/user.rules; then
  echo "To also close MouseTail's port in the firewall (asks for your password):"
  echo "    for net in $nets; do sudo ufw delete allow proto udp from \$net to any port 24802; done"
elif command -v firewall-cmd >/dev/null && firewall-cmd --list-rich-rules 2>/dev/null | grep -q 'port port="24802"'; then
  echo "To also close MouseTail's port in the firewall (asks for your password):"
  echo "    for net in $nets; do sudo firewall-cmd --permanent --remove-rich-rule=\"rule family=ipv4 source address=\$net port port=24802 protocol=udp accept\"; done; sudo firewall-cmd --reload"
fi
# enable-input.sh changed system settings (with your password), so undoing it needs it too.
rule=/etc/udev/rules.d/60-mousetail-uinput.rules
modules=/etc/modules-load.d/mousetail-uinput.conf
if [[ -e $rule || -e $modules ]]; then
  echo "To also undo enable-input.sh (asks for your password):"
  echo "    sudo rm -f $rule $modules"
fi
