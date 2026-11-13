{ lib, stdenv, rustPlatform, pkg-config, libxkbcommon, wayland, libGL, vulkan-loader, libx11, libxcursor, libxi, libxrandr, libxcb, fontconfig, freetype, sqlite }:
let
  manifest = (lib.importTOML ../Cargo.toml).workspace.package;
  runtimeLibs = [ libxkbcommon wayland libGL vulkan-loader libx11 libxcursor libxi libxrandr libxcb ];
in
rustPlatform.buildRustPackage {
  pname = "hirc";
  version = manifest.version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../crates ../macos ../README.md ../LICENSE ];
  };
  # gpui comes from one Zed git rev (pinned in Cargo.lock); fetching it
  # with builtins.fetchGit needs no per-crate hashes.
  cargoLock = {
    lockFile = ../Cargo.lock;
    allowBuiltinFetchGit = true;
  };
  cargoBuildFlags = [ "-p" "hirc" ];
  # CI runs the tests; some need a GPU, Ergo or bouncers.
  doCheck = false;
  nativeBuildInputs = [ pkg-config rustPlatform.bindgenHook ];
  # nixpkgs' cargoSetupHook sets LIBSQLITE3_SYS_USE_PKG_CONFIG, which overrides
  # rusqlite's `bundled` feature, so link nixpkgs' sqlite.
  buildInputs = [ sqlite ] ++ lib.optionals stdenv.hostPlatform.isLinux (runtimeLibs ++ [ fontconfig freetype ]);
  # gpui_apple's build script reads gpui's sources from ../gpui, its sibling in
  # Zed's workspace. The vendor dir names crates <name>-<version>, so point it there.
  preBuild = lib.optionalString stdenv.hostPlatform.isDarwin ''
    vendor="$NIX_BUILD_TOP/cargo-vendor-dir"
    gpui=$(basename "$vendor"/gpui-[0-9]*)
    substituteInPlace "$vendor"/gpui_apple-*/build.rs --replace-fail '.join("../gpui")' ".join(\"../$gpui\")"
  '';
  # On macOS, ship an app bundle so launchers and Finder see it; bin/ links to the binary inside.
  postInstall = lib.optionalString stdenv.hostPlatform.isDarwin ''
    app="$out/Applications/hirc.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    substitute macos/Info.plist "$app/Contents/Info.plist" --replace-fail @version@ ${manifest.version}
    cp macos/AppIcon.icns "$app/Contents/Resources/"
    mv $out/bin/hirc "$app/Contents/MacOS/hirc"
    ln -s "$app/Contents/MacOS/hirc" $out/bin/hirc
  '';
  postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/hirc
  '';
  meta = {
    description = manifest.description;
    license = lib.licenses.mit;
    mainProgram = "hirc";
    platforms = lib.platforms.darwin ++ lib.platforms.linux;
  };
}
