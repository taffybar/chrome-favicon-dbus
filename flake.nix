{
  description = "Chrome tab metadata bridge (HTTP from extension -> D-Bus for WM consumers)";

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
              pname = "chrome-window-dbus-bridge";
              inherit version;

              src = self;
              sourceRoot = "source/bridge-rs";

              cargoLock = {
                lockFile = ./bridge-rs/Cargo.lock;
              };

              meta = with pkgs.lib; {
                description = "HTTP ingest + D-Bus publisher for Chrome active tab metadata";
                license = licenses.mit;
                platforms = platforms.linux;
                mainProgram = "chrome-window-dbus-bridge";
              };
            };

          bridge = mkBridge pkgs.rustPlatform;
          bridgeStatic = mkBridge pkgs.pkgsStatic.rustPlatform;
        in
        {
          chrome-window-dbus-bridge = bridge;
          chrome-window-dbus-bridge-static = bridgeStatic;
          default = bridge;
        });

      apps = forAllSystems (system:
        let
          pkg = self.packages.${system}.chrome-window-dbus-bridge;
        in
        {
          default = {
            type = "app";
            program = "${pkg}/bin/chrome-window-dbus-bridge";
          };

          chrome-window-dbus-bridge = {
            type = "app";
            program = "${pkg}/bin/chrome-window-dbus-bridge";
          };
        });
    };
}
