{
  version ? null,
}:

let
  nixpkgs = builtins.getFlake "github:NixOS/nixpkgs/nixos-26.05";
  system = "x86_64-linux";
  pkgs = import nixpkgs {
    inherit system;
    config = {
      allowUnfree = true;
      nvidia.acceptLicense = true;
    };
  };
  addNvidia = version != null;
  nvidia_x11 =
    (
      (pkgs.linuxPackages.nvidiaPackages.mkDriver {
        inherit version;
        sha256_64bit = null;
        useSettings = false;
        usePersistenced = false;
      }).override
      { libsOnly = true; }
    ).overrideAttrs
      {
        src = builtins.fetchurl "https://us.download.nvidia.com/XFree86/Linux-x86_64/${version}/NVIDIA-Linux-x86_64-${version}.run";
      };
in
pkgs.buildEnv {
  name = "pond-nix-gpu";
  paths = [
    pkgs.mesa
    pkgs.libglvnd
    pkgs.libvdpau-va-gl
    pkgs.intel-media-driver
  ]
  ++ pkgs.lib.optionals addNvidia [
    nvidia_x11
    pkgs.nvidia-vaapi-driver
    pkgs.egl-wayland
    pkgs.egl-wayland2
    pkgs.egl-gbm
  ];
}
