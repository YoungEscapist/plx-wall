{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    # bindgen генерирует привязки к ffmpeg по РЕАЛЬНЫМ заголовкам системы
    # (см. build.rs): раскладка AVFrame/AVCodecContext меняется от версии к
    # версии, и руками её повторять нельзя. Хук проставляет LIBCLANG_PATH и
    # BINDGEN_EXTRA_CLANG_ARGS, без него bindgen не находит stddef.h.
    rustPlatform.bindgenHook
  ];

  buildInputs = with pkgs; [
    rustup
    wayland
    libxkbcommon
    gcc
    # Живые обои декодируются видеокартой прямо в dmabuf (см. hwvideo.rs).
    # ffmpeg нужен как БИБЛИОТЕКА, а не как процесс: кадр не должен покидать
    # видеопамять, а через трубу дочернего процесса он неизбежно поехал бы
    # в системную память сырым BGRA.
    ffmpeg
    libva
  ];

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    wayland
    libxkbcommon
    ffmpeg
    libva
  ];
}
