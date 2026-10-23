{
  version ? null,
  sha256 ? null,
}:

assert (version == null) == (sha256 == null);

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
    (pkgs.linuxPackages.nvidiaPackages.mkDriver {
      inherit version;
      sha256_64bit = sha256;
      useSettings = false;
      usePersistenced = false;
    }).override
      { libsOnly = true; };
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
