# nemu

A command-line tool for managing Nix-defined development VMs with QEMU.

## Install

Nemu needs Nix, which also supplies QEMU. The install script downloads the latest release for Apple Silicon Macs. On a Mac, run setup once to register the Linux VM that builds guest images. Then create a VM from your project's root folder; the project is shared into the guest at /workspace.

```sh
# Install Nix, then open a new terminal so it is on your PATH
sh <(curl --proto '=https' --tlsv1.2 -L https://nixos.org/nix/install)

# Enable flakes and the nix command
mkdir -p ~/.config/nix
echo "experimental-features = nix-command flakes" >> ~/.config/nix/nix.conf

# Install nemu to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/terraceio/nemu/main/install.sh | sh

# Register the Linux builder (macOS only, asks for your password)
nemu setup

# In your project: write nemu/flake.nix and nemu/guest.nix
nemu init

# Build and start the VM, then run commands in it
nemu up
nemu shell
nemu exec -- uname -a
```
