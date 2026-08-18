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
            # openssl-src (vendored into SQLCipher's build) configures with perl.
            nativeBuildInputs = [ pkgs.perl ];
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
            # Overrides common's nativeBuildInputs — keep perl (openssl-src).
            nativeBuildInputs = [
              pkgs.cargo-tauri.hook
              pkgs.pkg-config
              pkgs.perl
            ] ++ lib.optionals pkgs.stdenv.isLinux [
              pkgs.wrapGAppsHook3
              pkgs.copyDesktopItems
            ];
            buildInputs = lib.optionals pkgs.stdenv.isLinux [
              pkgs.webkitgtk_4_1
              pkgs.gtk3
              pkgs.libsoup_3
              pkgs.openssl
              pkgs.glib-networking
            ];
            tauriBuildFlags = [ "--no-bundle" ];
            # Linux desktops list apps from .desktop files; --no-bundle only gives
            # us a binary, so install the entry + hicolor icons ourselves. The file
            # is named after the binary on purpose: with no GTK app id set, both the
            # Wayland app_id and the X11 WM_CLASS come from the program name, so
            # launchers match the running window to this entry (and its icon).
            desktopItems = lib.optionals pkgs.stdenv.isLinux [
              (pkgs.makeDesktopItem {
                name = "memorious-desktop";
                desktopName = "Memorious";
                genericName = "Journal";
                comment = "Local-first capture: text, audio, photos";
                exec = "memorious-desktop";
                icon = "memorious";
                terminal = false;
                startupWMClass = "memorious-desktop";
                categories = [ "Utility" "Office" ];
                keywords = [ "journal" "notes" "capture" "diary" ];
              })
            ];
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
            postInstall = lib.optionalString pkgs.stdenv.isLinux ''
              icons=${self}/apps/desktop/src-tauri/icons
              install -Dm644 $icons/32x32.png       $out/share/icons/hicolor/32x32/apps/memorious.png
              install -Dm644 $icons/128x128.png     $out/share/icons/hicolor/128x128/apps/memorious.png
              install -Dm644 $icons/128x128@2x.png  $out/share/icons/hicolor/256x256/apps/memorious.png
              install -Dm644 $icons/icon.png        $out/share/icons/hicolor/512x512/apps/memorious.png
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
