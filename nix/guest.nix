{ pkgs, lib, nemu, ... }:
let
  uid = @HOST_UID@;
in
{
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  boot.kernelPackages = pkgs.linuxPackages_latest;
  boot.kernelParams = [
    (if pkgs.stdenv.hostPlatform.isAarch64 then "console=ttyAMA0,115200" else "console=ttyS0,115200")
  ];
  networking.hostName = "nemu";
  networking.firewall.enable = false;

  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  nix.settings.trusted-users = [ "root" "@wheel" ];
  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "no";
    };
    hostKeys = [ { path = "/run/nemu-keys/ssh_host_ed25519_key"; type = "ed25519"; } ];
  };

  fileSystems = {
    ${nemu.projectMount} = {
      device = "nemu_project";
      fsType = "9p";
      options = [ "trans=virtio" "version=9p2000.L" "msize=1048576" ];
    };
    "/run/nemu-bootstrap" = {
      device = "nemu_bootstrap";
      fsType = "9p";
      options = [ "trans=virtio" "version=9p2000.L" "ro" ];
    };
  } // lib.listToAttrs (lib.imap0 (index: share: {
    name = share.guest;
    value = {
      device = "nemu_share${toString index}";
      fsType = "9p";
      options = [ "trans=virtio" "version=9p2000.L" "msize=1048576" ]
        ++ lib.optional (share.readOnly or false) "ro";
    };
  }) (nemu.shares or [ ]));
  systemd.services.nemu-keys = {
    wantedBy = [ "multi-user.target" ];
    before = [ "sshd-keygen.service" "sshd.service" ];
    requiredBy = [ "sshd-keygen.service" "sshd.service" ];
    unitConfig.RequiresMountsFor = "/run/nemu-bootstrap";
    serviceConfig.Type = "oneshot";
    script = ''
      install -d -m 0700 /run/nemu-keys
      install -m 0600 /run/nemu-bootstrap/ssh_host_ed25519_key /run/nemu-keys/ssh_host_ed25519_key
      install -d -m 0700 -o ${lib.escapeShellArg nemu.user} -g users ${lib.escapeShellArg "/home/${nemu.user}/.ssh"}
      install -m 0600 -o ${lib.escapeShellArg nemu.user} -g users /run/nemu-bootstrap/authorized_keys ${lib.escapeShellArg "/home/${nemu.user}/.ssh/authorized_keys"}
    '';
  };
  users.users.${nemu.user} = {
    inherit uid;
    isNormalUser = uid >= 1000;
    isSystemUser = uid < 1000;
    group = "users";
    createHome = true;
    home = "/home/${nemu.user}";
    shell = pkgs.bashInteractive;
    extraGroups = [ "wheel" ];
  };
  security.sudo.wheelNeedsPassword = false;
  system.stateVersion = "26.05";
}
