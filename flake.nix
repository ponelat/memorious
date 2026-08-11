{
  description = "Infinite Journal v2 — local-first append-only capture, peers over iroh";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          lib = pkgs.lib;

          common = {
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            doCheck = false; # tests spin up live iroh endpoints — no sandbox network
          };
        in
        rec {
          memorious-cli = pkgs.rustPlatform.buildRustPackage (common // {
            pname = "memorious-cli";
            cargoBuildFlags = [ "-p" "memorious-core" "--bin" "memorious" ];
          });

          memorious-server = pkgs.rustPlatform.buildRustPackage (common // {
            pname = "memorious-server";
            cargoBuildFlags = [ "-p" "memorious-server" ];
            postInstall = ''
              mkdir -p $out/share/memorious
              cp -r ${self}/apps/web/dist $out/share/memorious/web
            '';
          });

          memorious-desktop = pkgs.rustPlatform.buildRustPackage (common // {
            pname = "memorious-desktop";
            buildAndTestSubdir = "apps/desktop/src-tauri";
            nativeBuildInputs = [
              pkgs.cargo-tauri.hook
              pkgs.pkg-config
            ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.wrapGAppsHook3 ];
            buildInputs = lib.optionals pkgs.stdenv.isLinux [
              pkgs.webkitgtk_4_1
              pkgs.gtk3
              pkgs.libsoup_3
              pkgs.openssl
              pkgs.glib-networking
            ];
            tauriBuildFlags = [ "--no-bundle" ];
            # We build with --no-bundle, so the hook's installPhase (which mv's
            # bundle output) has nothing to move — defining installPhase makes
            # the hook skip its own, and we install the plain binary instead.
            installPhase = ''
              runHook preInstall
              find target -type f -name memorious-desktop -path '*/release/*' \
                -exec install -Dm755 {} $out/bin/memorious-desktop \; -quit
              test -x "$out/bin/memorious-desktop"
              runHook postInstall
            '';
          });

          default = memorious-cli;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.memorious-cli}/bin/memorious";
        };
        desktop = {
          type = "app";
          program = "${self.packages.${system}.memorious-desktop}/bin/memorious-desktop";
        };
      });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.rustc pkgs.pkg-config pkgs.bun ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.webkitgtk_4_1
                pkgs.gtk3
                pkgs.libsoup_3
                pkgs.openssl
              ];
          };
        });
    };
}
