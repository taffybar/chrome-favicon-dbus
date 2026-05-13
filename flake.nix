{
  description = "Chrome favicon metadata over D-Bus for WM consumers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      cargoToml = builtins.fromTOML (builtins.readFile ./bridge-rs/Cargo.toml);
      version = cargoToml.package.version;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };

          mkBridge = rustPlatform:
            rustPlatform.buildRustPackage {
              pname = "chrome-favicon-dbus";
              inherit version;

              src = self;
              sourceRoot = "source/bridge-rs";

              cargoLock = {
                lockFile = ./bridge-rs/Cargo.lock;
              };

              meta = with pkgs.lib; {
                description = "HTTP ingest + D-Bus publisher for Chrome favicon metadata";
                license = licenses.mit;
                platforms = platforms.linux;
                mainProgram = "chrome-favicon-dbus";
              };
            };

          bridge = mkBridge pkgs.rustPlatform;
          bridgeStatic = mkBridge pkgs.pkgsStatic.rustPlatform;
        in
        {
          chrome-favicon-dbus = bridge;
          chrome-favicon-dbus-static = bridgeStatic;
          chrome-window-dbus-bridge = bridge;
          chrome-window-dbus-bridge-static = bridgeStatic;
          default = bridge;
        });

      apps = forAllSystems (system:
        let
          pkg = self.packages.${system}.chrome-favicon-dbus;
        in
        {
          default = {
            type = "app";
            program = "${pkg}/bin/chrome-favicon-dbus";
          };

          chrome-favicon-dbus = {
            type = "app";
            program = "${pkg}/bin/chrome-favicon-dbus";
          };

          chrome-window-dbus-bridge = {
            type = "app";
            program = "${pkg}/bin/chrome-favicon-dbus";
          };
        });
    };
}
