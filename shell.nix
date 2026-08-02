{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustup
    pkg-config
    wayland
    libxkbcommon
    gcc
  ];

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    wayland
    libxkbcommon
  ];
}
