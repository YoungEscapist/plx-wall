//! Привязки к ffmpeg для аппаратного декодирования живых обоев.
//!
//! Почему bindgen, а не рукописный FFI и не крейт `ffmpeg-next`:
//!
//! * `AVCodecContext` и `AVFrame` — огромные структуры, и смещения их полей
//!   меняются между версиями ffmpeg. Рукописное объявление молча разъедется
//!   при первом же обновлении канала nixpkgs и даст не ошибку компиляции, а
//!   чтение мусора из чужого поля.
//! * `ffmpeg-next` за версиями не поспевает: в системе стоит ffmpeg 9.0
//!   (libavutil 61), а крейт на момент написания знает максимум 7.x.
//!
//! bindgen читает те самые заголовки, с которыми мы и слинкуемся, поэтому
//! раскладка всегда верна — какой бы ffmpeg ни оказался в системе.

use std::{env, path::PathBuf};

fn main() {
    // ── Куда линковаться ────────────────────────────────────────────────────
    let mut include_args: Vec<String> = Vec::new();
    for lib in ["libavcodec", "libavformat", "libavutil", "libswscale"] {
        let found = pkg_config::Config::new()
            .probe(lib)
            .unwrap_or_else(|e| panic!("dwall: не найден {lib}: {e}"));
        for p in &found.include_paths {
            include_args.push(format!("-I{}", p.display()));
        }
        // На NixOS у собранного cargo бинаря пустой RUNPATH, а /usr/lib не
        // существует вовсе: без явного rpath dwall упал бы при запуске на
        // «libavcodec.so.63: cannot open shared object file». Прописываем
        // абсолютный путь стора — он же тот, с которым мы слинковались.
        //
        // Путь живёт до nix-collect-garbage. Чтобы сборщик мусора не унёс его
        // из-под готового бинаря, на ffmpeg стоит повесить gc-корень — так же,
        // как это сделано для rustup.
        for p in &found.link_paths {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
        }
    }

    // ── Собственно привязки ─────────────────────────────────────────────────
    // Берём ТОЛЬКО то, что действительно вызываем: полный заголовок ffmpeg
    // разворачивается в десятки тысяч строк, и каждая пересборка платила бы за
    // них временем.
    let bindings = bindgen::Builder::default()
        .header_contents(
            "wrapper.h",
            r#"
            #include <libavcodec/avcodec.h>
            #include <libavformat/avformat.h>
            #include <libavutil/hwcontext.h>
            #include <libavutil/hwcontext_drm.h>
            #include <libavutil/pixdesc.h>
            #include <libavutil/imgutils.h>
            #include <libswscale/swscale.h>
            "#,
        )
        .clang_args(&include_args)
        // Типы, чью раскладку мы читаем полями.
        .allowlist_type("AVFormatContext")
        .allowlist_type("AVCodecContext")
        .allowlist_type("AVCodecParameters")
        .allowlist_type("AVFrame")
        .allowlist_type("AVPacket")
        .allowlist_type("AVStream")
        .allowlist_type("AVCodec")
        .allowlist_type("AVBufferRef")
        .allowlist_type("AVHWFramesContext")
        .allowlist_type("AVDRMFrameDescriptor")
        .allowlist_type("AVDRMObjectDescriptor")
        .allowlist_type("AVDRMLayerDescriptor")
        .allowlist_type("AVDRMPlaneDescriptor")
        .allowlist_type("AVPixelFormat")
        .allowlist_type("AVHWDeviceType")
        .allowlist_type("AVRational")
        .allowlist_type("AVMediaType")
        // Функции.
        .allowlist_function("avformat_.*")
        .allowlist_function("avcodec_.*")
        .allowlist_function("av_read_frame")
        .allowlist_function("av_seek_frame")
        .allowlist_function("av_find_best_stream")
        .allowlist_function("av_packet_.*")
        .allowlist_function("av_frame_.*")
        .allowlist_function("av_buffer_.*")
        .allowlist_function("av_hwdevice_.*")
        .allowlist_function("av_hwframe_.*")
        .allowlist_function("sws_.*")
        .allowlist_type("SwsContext")
        .allowlist_var("SWS_.*")
        .allowlist_function("av_strerror")
        .allowlist_function("av_log_set_level")
        .allowlist_var("AV_.*")
        .allowlist_var("AVSEEK_.*")
        .allowlist_var("AVERROR.*")
        .allowlist_var("FF_.*")
        // enum'ы как обычные константы: так их проще сравнивать с полями,
        // которые ffmpeg объявляет как int.
        .default_enum_style(bindgen::EnumVariation::Consts)
        .derive_default(false)
        .layout_tests(false)
        .generate()
        .expect("dwall: bindgen не смог разобрать заголовки ffmpeg");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    bindings
        .write_to_file(out.join("ffmpeg.rs"))
        .expect("dwall: не записались привязки ffmpeg");

    println!("cargo:rerun-if-changed=build.rs");
}
