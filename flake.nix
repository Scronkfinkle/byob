{
  description = "yah — yet another harness: a native Rust AI agent harness targeting any inference URL";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        inherit (pkgs) lib stdenv;
        isLinux = stdenv.hostPlatform.isLinux;

        # Single source of truth for the toolchain, shared with rustup users.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # Linked at build time (rfd -> gtk3 / dbus file dialogs).
        buildInputs = lib.optionals isLinux [
          pkgs.gtk3
          pkgs.dbus
        ];

        # dlopen'ed at runtime by winit / wgpu, so they must be on LD_LIBRARY_PATH.
        runtimeLibs = lib.optionals isLinux [
          pkgs.wayland
          pkgs.libxkbcommon
          pkgs.vulkan-loader
          pkgs.libGL
          pkgs.libx11
          pkgs.libxcursor
          pkgs.libxi
          pkgs.libxrandr
        ];

        hasWorkspace = builtins.pathExists ./Cargo.lock;
        cargoToml = lib.importTOML ./Cargo.toml;

        # The browser UI: yah-web compiled to wasm + wasm-bindgen glue + index.html.
        yah-web = rustPlatform.buildRustPackage {
          pname = "yah-web";
          version = cargoToml.workspace.package.version;
          src = lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [
            pkgs.wasm-bindgen-cli
            pkgs.binaryen
          ];
          doCheck = false;
          buildPhase = ''
            runHook preBuild
            cargo build --release --offline --target wasm32-unknown-unknown -p yah-web
            mkdir -p dist
            wasm-bindgen --target web --no-typescript --out-dir dist target/wasm32-unknown-unknown/release/yah-web.wasm
            wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int -o dist/yah-web_bg.wasm dist/yah-web_bg.wasm
            cp apps/yah-web/index.html dist/
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            cp -r dist $out
            runHook postInstall
          '';
        };

        yah = rustPlatform.buildRustPackage {
          pname = "yah";
          version = cargoToml.workspace.package.version;
          src = lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "yah" ];
          # The full suite runs via `cargo xtask ci` in the dev shell.
          doCheck = false;
          nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals isLinux [ pkgs.makeWrapper ];
          inherit buildInputs;
          postFixup = lib.optionalString isLinux ''
            wrapProgram $out/bin/yah --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
              --set-default YAH_WEB_DIR ${yah-web}
          '';
          meta = {
            description = "Native Rust AI agent harness";
            license = with lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "yah";
          };
        };
      in
      {
        packages = lib.optionalAttrs hasWorkspace {
          inherit yah yah-web;
          default = yah;
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [
            rustToolchain
            pkgs.pkg-config
          ];
          buildInputs = buildInputs ++ runtimeLibs;
          packages = [
            pkgs.cargo-nextest
            pkgs.cargo-deny
            pkgs.cargo-watch
            # `cargo xtask web`: must match the wasm-bindgen version in Cargo.lock (xtask checks).
            pkgs.wasm-bindgen-cli
            pkgs.binaryen # wasm-opt for release web builds
            pkgs.xvfb # scripts/headless.sh: run the desktop app on a virtual display
            pkgs.nixfmt
          ];
          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };

        formatter = pkgs.nixfmt;
      }
    );
}
