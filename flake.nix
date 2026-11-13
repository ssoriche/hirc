{
  description = "A keyboard-first IRC client with IRCv3 history, reactions and replies";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAll (pkgs: rec {
        hirc = pkgs.callPackage ./nix/package.nix { };
        default = hirc;
      });
      apps = forAll (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/hirc";
          meta.description = "Run hirc";
        };
      });
      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
