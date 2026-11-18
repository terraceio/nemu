{ nixpkgsPath, hostSystem }:
let
  pkgs = import (builtins.toPath nixpkgsPath) { system = hostSystem; };
  stock = pkgs.darwin.linux-builder;
  managed = stock.override {
    modules = [ ({ lib, ... }: {
      virtualisation.darwin-builder.diskSize = 64 * 1024;
      virtualisation.qemu.networkingOptions = lib.mkForce [
        "-net nic,netdev=user.0,model=virtio"
        "-netdev user,id=user.0,hostfwd=tcp:127.0.0.1:31122-:22"
      ];
      virtualisation.qemu.options = lib.mkForce
        ((builtins.filter (option: option != "-nographic")
          stock.nixosConfig.virtualisation.qemu.options)
        ++ [ "-display none" "-serial file:serial.log" "-monitor none" ]);
    }) ];
  };
in
assert managed.nixosConfig.system.build.toplevel == stock.nixosConfig.system.build.toplevel;
{
  runner = managed.nixosConfig.system.build.vm;
  spec = {
    guest = toString stock.nixosConfig.system.build.toplevel;
    runner = toString managed.nixosConfig.system.build.vm;
    executable = "run-${managed.nixosConfig.system.name}-vm";
    hostKey = builtins.readFile stock.nixosConfig.environment.etc."ssh/ssh_host_ed25519_key.pub".source;
  };
}
