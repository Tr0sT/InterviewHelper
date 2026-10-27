{
  description = "Interview Helper — continuous meeting capture and Codex assistance";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/f20cd90e28fe0403873f473b29e42662cd86e8c4";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      libraries = with pkgs; [ gtk3 webkitgtk_4_1 libayatana-appindicator openssl ];
    in {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "interview-helper";
        version = "0.1.0";
        src = pkgs.lib.cleanSourceWith {
          src = self;
          filter = path: type:
            let
              relative = pkgs.lib.removePrefix (toString self + "/") (toString path);
              first = builtins.head (pkgs.lib.splitString "/" relative);
            in toString path == toString self ||
              (builtins.elem first [ "src-tauri" "ui" "THIRD_PARTY_NOTICES.md" ] &&
               !(builtins.elem (baseNameOf path) [ "target" ]));
        };
        cargoRoot = "src-tauri";
        buildAndTestSubdir = "src-tauri";
        cargoLock.lockFile = ./src-tauri/Cargo.lock;
        nativeBuildInputs = with pkgs; [ pkg-config wrapGAppsHook3 ];
        buildInputs = libraries;
        cargoTestFlags = [ "--no-default-features" "--lib" ];
        postInstall = ''
          install -Dm644 $src/src-tauri/icons/icon.png $out/share/icons/hicolor/64x64/apps/interview-helper.png
          install -Dm644 $src/THIRD_PARTY_NOTICES.md $out/share/doc/interview-helper/THIRD_PARTY_NOTICES.md
          mkdir -p $out/share/applications
          cat > $out/share/applications/interview-helper.desktop <<EOF
          [Desktop Entry]
          Type=Application
          Name=Interview Helper
          Comment=Meeting capture and Codex assistance
          Exec=$out/bin/interview-helper
          Icon=interview-helper
          Terminal=false
          Categories=AudioVideo;Audio;Utility;
          StartupWMClass=interview-helper
          EOF
        '';
        preFixup = ''
          gappsWrapperArgs+=(
            --prefix PATH : "${pkgs.lib.makeBinPath [ pkgs.ffmpeg pkgs.pulseaudio pkgs.xdg-utils ]}"
            --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath [ pkgs.libayatana-appindicator ]}"
            --set WEBKIT_DISABLE_DMABUF_RENDERER 1
          )
        '';
        meta.mainProgram = "interview-helper";
      };
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc clippy rustfmt pkg-config nodejs ffmpeg pulseaudio ];
        buildInputs = libraries;
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libraries;
        XDG_DATA_DIRS = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:${pkgs.hicolor-icon-theme}/share";
        WEBKIT_DISABLE_DMABUF_RENDERER = "1";
      };
    };
}
