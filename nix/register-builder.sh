test "$(id -u)" = 0
test ! -L /etc/nix/nix.conf || { echo 'nix.conf is managed by another tool; add the Nemu builder through that configuration instead.' >&2; exit 1; }
test ! -L /etc/nix/nemu
test ! -L /etc/nix/nemu.conf
if test -e /etc/nix/nemu.conf; then
  test "$(head -n 1 /etc/nix/nemu.conf)" = '# Managed by nemu setup' || { echo 'Refusing to overwrite unmanaged nemu.conf' >&2; exit 1; }
elif test -e /etc/nix/nemu; then
  echo 'Refusing to overwrite an unmanaged /etc/nix/nemu directory' >&2
  exit 1
fi
launchctl print system/org.nixos.nix-daemon >/dev/null
install -d -m 755 /etc/nix/nemu
printf '%s\n' '# Managed by nemu setup' "$3" > /etc/nix/nemu.conf
install -m 600 "$1" /etc/nix/nemu/key
install -m 644 "$1.pub" /etc/nix/nemu/key.pub
printf '%s' "$2" > /etc/nix/nemu/machines
chmod 644 /etc/nix/nemu.conf /etc/nix/nemu/machines
if ! grep -Fqx '!include nemu.conf' /etc/nix/nix.conf; then
  printf '\n!include nemu.conf\n' >> /etc/nix/nix.conf
fi
launchctl kickstart -k system/org.nixos.nix-daemon
