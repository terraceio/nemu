{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { nixpkgs, ... }:
    let
      hosts = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
    in {
      nemu = nixpkgs.lib.genAttrs hosts (host:
        let
          arch = builtins.head (nixpkgs.lib.splitString "-" host);
          system = "${arch}-linux";
          hostPkgs = import nixpkgs { system = host; };
          config = {
            inherit system;
            cpus = 2;
            memoryMiB = 4096;
            diskGiB = 32;
            numaNodes = 1;
            sshPort = 2229;
            ports = [];
            user = "dev";
            projectMount = "/workspace";
            shares = [];
          };
          guest = nixpkgs.lib.nixosSystem {
            inherit system;
            specialArgs.nemu = config;
            modules = [
              ./guest.nix
              "${nixpkgs}/nixos/modules/virtualisation/disk-image.nix"
            ];
          };
        in {
          inherit config;
          tools = hostPkgs.symlinkJoin {
            name = "nemu-host-tools";
            paths = [ (hostPkgs.qemu.override { hostCpuOnly = true; }) ];
          };
          image = guest.config.system.build.image;
          toplevel = guest.config.system.build.toplevel;
        });
    };
}
