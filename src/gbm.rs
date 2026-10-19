//! Кадры живых обоев в линейных буферах GBM — dmabuf вместо wl_shm.
//!
//! ## Зачем
//!
//! Кадр, отданный композитору через wl_shm, композитор обязан сам залить в
//! текстуру — и делает это в ГЛАВНОМ потоке, на каждый кадр ролика. Замер
//! 19.10.2026 на NixithOS (RTX 5060 под nouveau, GL через zink): ролик
//! 2560×1600@60 стоил parallax 40 мс на такт в среднем и 62 мс в худшем —
//! сам композитор проседал до ~25 кадров/с, и «лагали» не только обои, а всё.
//! Статичная картинка в том же замере — 0.08 мс.
//!
//! Буфер GBM живёт в памяти видеокарты (или там, откуда она читает сама), и
//! композитор берёт его как есть, без заливки. Пиксели пишет ffmpeg через нашу
//! трубу прямо в отображение буфера — копирований не прибавилось: раньше
//! `splice` клал их в memfd, теперь `read` кладёт их сюда.
//!
//! ## Когда не выходит
//!
//! Нет libgbm-устройства, композитор не объявил ARGB8888 с линейным
//! модификатором, шаг строки у буфера не равен `ширина·4` (рисующий код
//! считает кадр плотным) или `PLX_WALL_SHM=1` — тогда [`буфер`] отдаёт
//! `None`, и кадры идут прежним путём через wl_shm.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: libc::c_int) -> *mut c_void;
    fn gbm_bo_create(dev: *mut c_void, w: u32, h: u32, format: u32, flags: u32) -> *mut c_void;
    fn gbm_bo_get_fd(bo: *mut c_void) -> libc::c_int;
    fn gbm_bo_get_stride(bo: *mut c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: libc::c_int) -> u32;
    fn gbm_bo_destroy(bo: *mut c_void);
}

/// `DRM_FORMAT_ARGB8888` ('AR24'): в памяти B, G, R, A — ровно `bgra` ffmpeg.
pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// `DRM_FORMAT_MOD_LINEAR`.
pub const ЛИНЕЙНЫЙ: u64 = 0;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

/// `DMA_BUF_IOCTL_SYNC` и его флаги (linux/dma-buf.h).
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_WRITE: u64 = 2;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;

/// Устройство GBM на рендер-узле — одно на процесс, живёт до конца.
fn устройство() -> Option<*mut c_void> {
    static УСТРОЙСТВО: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    УСТРОЙСТВО
        .get_or_init(|| {
            let путь = std::ffi::CString::new(crate::hwvideo::УЗЕЛ_ПО_УМОЛЧАНИЮ).ok()?;
            let fd = unsafe { libc::open(путь.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            // Дескриптор нарочно не закрываем: устройство GBM держит его всю жизнь.
            let dev = unsafe { gbm_create_device(fd) };
            if dev.is_null() {
                unsafe { libc::close(fd) };
                eprintln!("plx-wall: gbm_create_device не удался — кадры пойдут через wl_shm");
                return None;
            }
            Some(dev as usize)
        })
        .map(|p| p as *mut c_void)
}

/// Отображение одного буфера в память. Держат его и слот, и декодер, пока
/// заливает кадр (через `Arc`): обои могут смениться посреди заливки, и
/// отображение не должно исчезнуть из-под пишущего потока.
pub struct Отображение {
    pub fd: OwnedFd,
    pub ptr: *mut u8,
    pub len: usize,
    pub шаг: u32,
    pub сдвиг: u32,
}

// Сырой указатель на общее отображение: пишет его ровно один поток за раз
// (слот одолжен декодеру — главный цикл его не трогает, см. `Заём`).
unsafe impl Send for Отображение {}
unsafe impl Sync for Отображение {}

impl Drop for Отображение {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut c_void, self.len) };
    }
}

impl Отображение {
    /// Залить в буфер `размер` байт из трубы. Ложь — труба кончилась.
    pub fn залить(&self, труба: &impl AsRawFd, размер: usize) -> bool {
        if размер > self.len {
            return false;
        }
        синхронизировать(&self.fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE);
        let mut залито = 0usize;
        let итог = loop {
            if залито >= размер {
                break true;
            }
            let n = unsafe {
                libc::read(
                    труба.as_raw_fd(),
                    self.ptr.add(залито) as *mut c_void,
                    размер - залито,
                )
            };
            match n {
                0 => break false,
                n if n < 0 => {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    eprintln!("plx-wall: чтение кадра в буфер GBM не удалось: {e}");
                    break false;
                }
                n => залито += n as usize,
            }
        };
        синхронизировать(&self.fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE);
        итог
    }
}

fn синхронизировать(fd: &OwnedFd, флаги: u64) {
    let флаги = флаги;
    unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &флаги as *const u64) };
}

/// Завести линейный буфер `w`×`h` ARGB8888 и отобразить его в память.
/// `None` — GBM нет, или шаг строки не плотный (см. доктекст модуля).
pub fn буфер(w: u32, h: u32) -> Option<Arc<Отображение>> {
    if std::env::var_os("PLX_WALL_SHM").is_some() || w == 0 || h == 0 {
        return None;
    }
    let dev = устройство()?;
    let bo = unsafe {
        gbm_bo_create(dev, w, h, ARGB8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING)
    };
    if bo.is_null() {
        eprintln!("plx-wall: gbm_bo_create {w}x{h} не удался — кадры пойдут через wl_shm");
        return None;
    }
    let (fd, шаг, сдвиг) = unsafe { (gbm_bo_get_fd(bo), gbm_bo_get_stride(bo), gbm_bo_get_offset(bo, 0)) };
    // Сам bo больше не нужен: dmabuf держит память, пока открыт дескриптор.
    unsafe { gbm_bo_destroy(bo) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if шаг != w * 4 || сдвиг != 0 {
        eprintln!("plx-wall: у буфера GBM шаг {шаг} при ширине {w} — кадры пойдут через wl_shm");
        return None;
    }
    let len = (шаг as usize) * (h as usize);
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        eprintln!(
            "plx-wall: отобразить буфер GBM не вышло ({}) — кадры пойдут через wl_shm",
            std::io::Error::last_os_error()
        );
        return None;
    }
    Some(Arc::new(Отображение { fd, ptr: ptr as *mut u8, len, шаг, сдвиг }))
}
