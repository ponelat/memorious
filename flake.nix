{
  description = "Memorious — local-first append-only capture, peers over iroh";

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
            version = "0.3.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            doCheck = false; # tests spin up live iroh endpoints — no sandbox network
            # crates/core/build.rs stamps every binary with its git commit and
            # compile date (Status.git_sha/build_date, CLI --version); the
            # sandboxed `src = self` above has no `.git` for it to ask (same
            # reason `version` here is hand-maintained, not `self.rev` — see
            # LOG.md 2026-09-17), so give it the commit directly. Build date is
            # left to build.rs's own `date` fallback (present in stdenv).
            MEMORIOUS_GIT_SHA = self.shortRev or self.dirtyShortRev or "unknown";
            # openssl-src (vendored into SQLCipher's build) configures with perl.
            nativeBuildInputs = [ pkgs.perl ];
          };
        in
        rec {
          # Linux-specific microphone/key worker used by the desktop's optional
          # background PTT feature. It deliberately has no journal endpoint or
          # credential: its only delivery path is the daemon Unix socket.
          memorious-ptt = pkgs.writeShellApplication {
            name = "memorious-ptt";
            runtimeInputs = [
              (pkgs.python3.withPackages (ps: [ ps.evdev ps.requests ]))
              pkgs.pipewire
              pkgs.ffmpeg-headless
              pkgs.whisper-cpp
            ];
            text = ''
              exec python ${self}/apps/desktop/ptt/memorious-ptt.py "$@"
            '';
          };

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
            buildAndTestSubdir = if pkgs.stdenv.isLinux then "apps/desktop/linux" else "apps/desktop/src-tauri";
            cargoBuildFlags = lib.optionals pkgs.stdenv.isLinux [ "-p" "memorious-linux" ];
            # Overrides common's nativeBuildInputs — keep perl (openssl-src).
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.perl
            ] ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.cargo-tauri.hook ]
              ++ lib.optionals pkgs.stdenv.isLinux [
              pkgs.wrapGAppsHook4
              pkgs.copyDesktopItems
            ];
            buildInputs = lib.optionals pkgs.stdenv.isLinux [
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.gst_all_1.gst-plugins-good
              pkgs.gst_all_1.gst-libav
              pkgs.openssl
              pkgs.glib-networking
              memorious-ptt
            ];
            tauriBuildFlags = lib.optionals pkgs.stdenv.isDarwin [ "--no-bundle" ];
            # Match the GTK application id so launchers associate the native window.
            desktopItems = lib.optionals pkgs.stdenv.isLinux [
              (pkgs.makeDesktopItem {
                name = "app.memorious";
                desktopName = "Memorious";
                genericName = "Journal";
                comment = "Local-first capture: text, audio, photos";
                exec = "memorious-desktop";
                icon = "memorious";
                terminal = false;
                startupWMClass = "app.memorious";
                categories = [ "Utility" "Office" ];
                keywords = [ "journal" "notes" "capture" "diary" ];
              })
            ];
            # Both shells install the plain executable; macOS uses --no-bundle.
            installPhase = ''
              runHook preInstall
              find target -type f -name memorious-desktop -path '*/release/*' \
                -exec install -Dm755 {} $out/bin/memorious-desktop \; -quit
              test -x "$out/bin/memorious-desktop"
              runHook postInstall
            '';
            postInstall = lib.optionalString pkgs.stdenv.isLinux ''
              install -Dm755 ${memorious-ptt}/bin/memorious-ptt $out/bin/memorious-ptt
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
            packages = [ pkgs.cargo pkgs.rustc pkgs.pkg-config pkgs.bun pkgs.perl ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.gtk4
                pkgs.libadwaita
                pkgs.openssl
              ];
          };
        });
    };
}
