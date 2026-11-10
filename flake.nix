{
  description = "byob — bring your own brain: a native Rust AI agent harness targeting any inference URL";

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

        # File dialogs (rfd) go through the XDG desktop portal over D-Bus, in pure Rust; zenity
        # is the fallback where no portal runs (some tiling WMs), so the package and shell carry it.
        dialogFallback = lib.optionals isLinux [ pkgs.zenity ];

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

        # The browser UI: byob-web compiled to wasm + wasm-bindgen glue + index.html.
        byob-web = rustPlatform.buildRustPackage {
          pname = "byob-web";
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
            cargo build --release --offline --target wasm32-unknown-unknown -p byob-web
            mkdir -p dist
            wasm-bindgen --target web --no-typescript --out-dir dist target/wasm32-unknown-unknown/release/byob-web.wasm
            wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int -o dist/byob-web_bg.wasm dist/byob-web_bg.wasm
            cp apps/byob-web/index.html dist/
            # Precompressed copies; the server sends them on Accept-Encoding: gzip.
            gzip -9 -k -n dist/byob-web_bg.wasm dist/byob-web.js
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            cp -r dist $out
            runHook postInstall
          '';
        };

        # The Servo helper that renders HTML app artifacts inline (host/artifact-host). Its own
        # workspace and lock file: MPL-2.0 + tokio live only in there, never linked into byob.
        # SpiderMonkey comes prebuilt from servo/mozjs releases, fetched here so the build is
        # offline (mozjs_sys reads MOZJS_ARCHIVE). Other systems: build it from source with
        # MOZJS_FROM_SOURCE=1 (needs clang, llvm, python3, m4, gnumake, which; AS=clang).
        mozjsArchive =
          {
            x86_64-linux = pkgs.fetchurl {
              url = "https://github.com/servo/mozjs/releases/download/mozjs-sys-v153.6.0-1/libmozjs-x86_64-unknown-linux-gnu.tar.gz";
              hash = "sha256-SwjUjpiDngWMZSPtED94dIdKz6V6HVTvk8rcIecWjzc=";
            };
          }
          .${system} or null;
        hasHost = mozjsArchive != null && builtins.pathExists ./host/artifact-host/Cargo.lock;

        byob-artifact-host = rustPlatform.buildRustPackage {
          pname = "byob-artifact-host";
          version = "0.1.0";
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./host/artifact-host/Cargo.toml
              ./host/artifact-host/Cargo.lock
              ./host/artifact-host/src
              ./host/artifact-host/NOTICE
              ./crates/artifact-wire
            ];
          };
          cargoRoot = "host/artifact-host";
          buildAndTestSubdir = "host/artifact-host";
          cargoLock.lockFile = ./host/artifact-host/Cargo.lock;
          MOZJS_ARCHIVE = mozjsArchive;
          # nixpkgs asks aws-lc-sys (rustls' crypto, via Servo) for a system AWS-LC; use the
          # bundled one, as the dev-shell build does. (The plain variable is reset to 1 by a
          # nixpkgs hook; the target-specific one wins.)
          AWS_LC_SYS_USE_SYSTEM_x86_64_unknown_linux_gnu = "0";
          # python3: Servo generates its DOM bindings at build time.
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.python3
            pkgs.makeWrapper
          ];
          buildInputs = [
            pkgs.fontconfig
            pkgs.freetype
            pkgs.zlib
            # nixpkgs builds point these -sys crates at system libraries.
            pkgs.zstd
            pkgs.sqlite
          ];
          # mozjs' glue crates run cbindgen (`cargo metadata`) inside the vendored crate dirs,
          # where the vendor copy's own relative source config would win; make it absolute.
          preBuild = ''
            vendor="''${cargoDepsCopy:-$NIX_BUILD_TOP/cargo-vendor-dir}"
            if [ -f "$vendor/.cargo/config.toml" ]; then
              chmod -R u+w "$vendor/.cargo"
              sed -i "s|directory = \"cargo-vendor-dir\"|directory = \"$vendor\"|" "$vendor/.cargo/config.toml"
            fi
            # ...and their packaged Cargo.lock pins versions newer than the vendored ones.
            for d in "$vendor"/mozjs_*; do
              if [ -f "$d/Cargo.lock" ]; then
                chmod -R u+w "$d"
                rm "$d/Cargo.lock"
                python3 -c 'import json,sys; p=sys.argv[1]; j=json.load(open(p)); j["files"].pop("Cargo.lock", None); json.dump(j, open(p, "w"))' "$d/.cargo-checksum.json"
              fi
            done
          '';
          doCheck = false;
          postInstall = ''
            install -Dm644 host/artifact-host/NOTICE $out/share/doc/byob-artifact-host/NOTICE
          '';
          # Software GL through EGL (Mesa llvmpipe), dlopen'ed.
          postFixup = ''
            wrapProgram $out/bin/byob-artifact-host --prefix LD_LIBRARY_PATH : ${
              lib.makeLibraryPath [ pkgs.libGL ]
            }
          '';
          meta = {
            description = "Renders byob's HTML app artifacts with Servo, in a separate process";
            # Our code is MIT/Apache-2.0; the binary links Servo and SpiderMonkey (MPL-2.0).
            license = with lib.licenses; [
              mit
              asl20
              mpl20
            ];
            mainProgram = "byob-artifact-host";
            platforms = [ "x86_64-linux" ];
          };
        };

        # The desktop app. `slim`: without the browser UI compiled in (`--no-default-features`).
        mkByob =
          {
            slim ? false,
          }:
          rustPlatform.buildRustPackage {
            pname = if slim then "byob-slim" else "byob";
            version = cargoToml.workspace.package.version;
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "byob"
            ]
            ++ lib.optionals slim [ "--no-default-features" ];
            # byob-web-assets embeds whatever is in crates/web-assets/assets/ at build time.
            preBuild = lib.optionalString (!slim) ''
              mkdir -p crates/web-assets/assets
              cp ${byob-web}/* crates/web-assets/assets/
            '';
            # The full suite runs via `cargo xtask ci` in the dev shell.
            doCheck = false;
            nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals isLinux [ pkgs.makeWrapper ];
            # nixpkgs' cargo hooks point libsqlite3-sys at the system SQLite (overriding
            # rusqlite's `bundled`), so it has to be an input here.
            buildInputs = [ pkgs.sqlite ];
            postFixup = lib.optionalString isLinux ''
              wrapProgram $out/bin/byob --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
                --suffix PATH : ${lib.makeBinPath dialogFallback} ${lib.optionalString hasHost "--set-default BYOB_ARTIFACT_HOST ${byob-artifact-host}/bin/byob-artifact-host"}
            '';
            meta = {
              description = "Native Rust AI agent harness" + lib.optionalString slim " (without the browser UI)";
              license = with lib.licenses; [
                mit
                asl20
              ];
              mainProgram = "byob";
            };
          };
        byob = mkByob { };
        byob-slim = mkByob { slim = true; };
      in
      {
        packages =
          lib.optionalAttrs hasWorkspace {
            inherit byob byob-slim byob-web;
            default = byob;
          }
          // lib.optionalAttrs hasHost { inherit byob-artifact-host; };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [
            rustToolchain
            pkgs.pkg-config
          ];
          buildInputs = runtimeLibs;
          packages = [
            pkgs.cargo-nextest
            pkgs.cargo-deny
            pkgs.cargo-watch
            # `cargo xtask web`: must match the wasm-bindgen version in Cargo.lock (xtask checks).
            pkgs.wasm-bindgen-cli
            pkgs.binaryen # wasm-opt for release web builds
            pkgs.xvfb # scripts/headless.sh: run the desktop app on a virtual display
            pkgs.nixfmt
          ]
          ++ dialogFallback;
          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };

        formatter = pkgs.nixfmt;
      }
    );
}
