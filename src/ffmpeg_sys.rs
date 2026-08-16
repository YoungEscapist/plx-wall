//! Сырые привязки к ffmpeg, сгенерированные bindgen'ом по заголовкам системы.
//!
//! Ничего рукописного здесь нет и быть не должно — см. build.rs. Безопасная
//! обёртка над этим модулем живёт в hwvideo.rs.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]

include!(concat!(env!("OUT_DIR"), "/ffmpeg.rs"));
