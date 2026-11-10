# Throwaway NixOS VM: vicodec + lavapipe, runs /mnt/share/run.sh at boot
# and powers off. See run-vm.sh.
{ shareDir }:
{ pkgs, lib, modulesPath, ... }:
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];
  # Built by run-vm.sh; runs /mnt/share/run.sh at boot, then powers off.
  system.stateVersion = "25.11";
  boot.kernelPackages = pkgs.linuxPackages_latest;
  boot.kernelModules = [ "vicodec" ];
  boot.extraModulePackages = [ (pkgs.callPackage ./vicodec.nix { kernel = pkgs.linuxPackages_latest.kernel; }) ];
  boot.extraModprobeConfig = "options vicodec multiplanar=1";
  hardware.graphics.enable = true;
  virtualisation = {
    graphics = false;
    memorySize = 3072;
    cores = 4;
    diskImage = null;
    sharedDirectories.share = { source = shareDir; target = "/mnt/share"; };
  };
  environment.systemPackages = with pkgs; [ v4l-utils vulkan-tools ];
  users.users.root.password = "";
  systemd.services.selftest = {
    wantedBy = [ "multi-user.target" ];
    after = [ "multi-user.target" "systemd-modules-load.service" ];
    path = with pkgs; [ bash coreutils v4l-utils vulkan-tools gnugrep kmod ffmpeg ];
    environment = {
      LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
      VK_ICD_FILENAMES = "/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json";
    };
    serviceConfig = { Type = "oneshot"; StandardOutput = "journal+console"; };
    script = ''
      bash /mnt/share/run.sh > /mnt/share/result.log 2>&1 || true
      sync
      systemctl poweroff
    '';
  };
}
