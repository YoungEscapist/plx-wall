// dwall — wallpaper-клиент для dawn.
// Использование: dwall [путь к картинке]
// Сигнал USR1 → следующие обои
// Сигнал USR2 → показать/скрыть оверлей
//
// В dawn config.lua (Super+W уже занят toggle_floating, поэтому Alt):
//   bind{ mods = "alt", key = "w", action = "spawn",
//         cmd = "pkill -USR1 -x dwall" }
//   bind{ mods = "alt+shift", key = "w", action = "spawn",
//         cmd = "pkill -USR2 -x dwall" }

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

mod online;
mod palette;

use image::{DynamicImage, GenericImageView, imageops::FilterType};
use wayland_client::{
    Connection, Dispatch, QueueHandle, Proxy, WEnum,
    protocol::{
        wl_compositor::WlCompositor,
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::{self, WlShmPool},
        wl_surface::WlSurface,
        wl_buffer::WlBuffer,
        wl_region::WlRegion,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

// ── Константы UI ────────────────────────────────────────────────────────────

const UI_W: u32 = 740;
/// Высота меню: сверху отступ под заголовок, дальше VIS_ROWS рядов карточек.
/// Раньше было 480 — в них помещалось ровно два ряда, и всё, что дальше,
/// просто не существовало для пользователя: ui_cells обрывал список по краю
/// окна, а прокрутки не было.
const UI_H: u32 = 576;
const CORNER: u32 = 14;
const THUMB: u32 = 155;
const COLS: u32 = 4;
const GAP: u32 = 12;
/// Сколько рядов видно сразу. Остальные — прокруткой (колесо или стрелки).
const VIS_ROWS: u32 = 3;
/// Отступ сетки сверху.
const MY: u32 = 60;
/// Полоса прокрутки в правом поле.
const SB_W: u32 = 6;
const SB_COLOR: u32 = 0xFF3A3A3A;
const SB_THUMB_COLOR: u32 = 0xFFAAAAAA;
/// Стрелки прокрутки — кликабельные, если мыши без колеса.
const ARROW: u32 = 26;
/// Длительность плавного перехода между обоями.
const FADE_MS: u64 = 380;
const BG_COLOR: u32 = 0xCC111111; // ARGB
const CARD_COLOR: u32 = 0xFF222222;
const PLUS_COLOR: u32 = 0xFF333333;
const HOVER_COLOR: u32 = 0xFF444444;
const SEL_COLOR: u32 = 0xFFFFFFFF; // галочка
/// Сторона кнопки удаления в углу карточки.
const DEL_SIZE: u32 = 28;
const DEL_BG: u32 = 0xCC000000;
const DEL_FG: u32 = 0xFFDDDDDD;

/// Что показывает сетка меню.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Вкладка {
    /// Свои обои — то, что было в dwall всегда.
    Мои,
    /// Статичные картинки с wallhaven.
    Картинки,
    /// Живые обои с moewalls.
    Живые,
}

impl Вкладка {
    fn источник(self) -> Option<online::Источник> {
        match self {
            Вкладка::Мои => None,
            Вкладка::Картинки => Some(online::Источник::Картинки),
            Вкладка::Живые => Some(online::Источник::Живые),
        }
    }
}

/// Ответ фонового потока, ходившего в сеть.
enum ИзСети {
    /// Список находок вместе с уже разобранными миниатюрами.
    Список(Vec<online::Находка>, Vec<(u32, u32, Vec<u8>)>),
    /// Скачанные обои — путь к файлу.
    Скачано(std::path::PathBuf),
    Ошибка(String),
}

#[derive(Clone, Copy)]
enum UiAction {
    SelectWallpaper(usize),
    AddWallpaper,
    DeleteWallpaper(usize),
    Scroll(i32),
    /// Переключить вкладку сетки.
    Вкладка(Вкладка),
    /// Скачать и поставить обои из сетевой выдачи.
    ВзятьИзСети(usize),
    /// Спросить, что искать, и перезапросить выдачу.
    Поиск,
}

/// Плавный переход между обоями.
///
/// `from` — снимок ТОГО, ЧТО БЫЛО НА ЭКРАНЕ (готовый кадр из буфера показа), а
/// не исходная картинка: так один и тот же код работает и для статичных обоев,
/// и для видео, где «текущего кадра» в виде файла нет вообще.
///
/// `start` заводится не в момент клика, а когда пришёл ПЕРВЫЙ кадр новых
/// обоев: 4K-PNG декодируется сотни миллисекунд, и если пустить часы сразу,
/// переход успел бы кончиться до того, как появилось что показывать, — вместо
/// плавной смены был бы рывок.
struct Fade {
    from: Vec<u8>,
    start: Option<std::time::Instant>,
    /// Новых обоев ещё нет — они декодируются в фоне. Пока флаг стоит, часы
    /// не идут и смешивать нечего: на экране просто остаётся старый кадр.
    ждём: bool,
}

struct HitRect { x: u32, y: u32, w: u32, h: u32, action: UiAction }

/// Постоянный буфер разделяемой памяти под кадр.
///
/// `free` — снят ли буфер с показа. Композитор шлёт `wl_buffer.release`, когда
/// перестал читать буфер; писать в него раньше этого момента нельзя — на живых
/// обоях получаются рваные кадры (полосы соседнего кадра). См.
/// `Dispatch<WlBuffer, Arc<AtomicBool>>` и `take_free_slot`.
struct ShmSlot {
    buffer: WlBuffer,
    ptr: *mut u8,
    len: usize,
    free: std::sync::Arc<AtomicBool>,
}

impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len); }
    }
}

/// Завести пару постоянных буферов `w`×`h`. Пул уничтожаем сразу: отображение
/// держит сам wl_buffer, а живой пул на каждый кадр — это утечка и в памяти, и
/// в файловых дескрипторах КОМПОЗИТОРА (замер 04.08.2026: меню под курсором
/// добавляло dawn ~190 fd и 114 МБ за пару секунд, дальше он упирался в лимит
/// 1024 fd и рвал соединение).
fn make_slots(
    shm: &WlShm,
    qh: &QueueHandle<Config>,
    w: u32,
    h: u32,
) -> Option<Vec<ShmSlot>> {
    let size = (w * h * 4) as usize;
    let mut out = Vec::with_capacity(2);
    for _ in 0..2 {
        let (fd, ptr) = create_shm_fd(size)?;
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let pool = shm.create_pool(owned.as_fd(), size as i32, qh, ());
        let free = std::sync::Arc::new(AtomicBool::new(true));
        let buffer = pool.create_buffer(
            0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, free.clone(),
        );
        pool.destroy(); // отображение держит сам буфер
        out.push(ShmSlot { buffer, ptr: ptr as *mut u8, len: size, free });
    }
    Some(out)
}

/// Выбрать буфер, который композитор уже отпустил.
///
/// Если свободного нет (композитор ещё не прислал release), кадр пропускаем:
/// для обоев потерять кадр не страшно, а писать в показываемый буфер — это
/// видимые артефакты.
fn take_free_slot(slots: &[ShmSlot], last: usize) -> Option<usize> {
    for шаг in 1..=slots.len() {
        let i = (last + шаг) % slots.len();
        if slots[i].free.load(Ordering::Relaxed) {
            return Some(i);
        }
    }
    None
}

/// Проигрыватель живых обоев.
///
/// Кадры готовит фоновый поток и кладёт в канал ёмкостью 1 — он же служит
/// тормозом: пока главный цикл не забрал кадр, декодер стоит, и память не
/// растёт (у видео кадр в размер экрана — это 11 МБ, держать очередь нельзя).
/// Темп задаёт главный цикл по `delay`, а не декодер.
struct Anim {
    rx: std::sync::mpsc::Receiver<(u32, u32, Vec<u8>, u64, bool)>,
    next_at: std::time::Instant,
    /// Живой ffmpeg. В Drop его обязательно убиваем: иначе при смене обоев
    /// процессы копятся и продолжают декодировать в никуда.
    ///
    /// Ручку делим с потоком-читателем: при провале аппаратного декодера тот
    /// перезапускает процесс программным (см. start_anim), и убивать надо уже
    /// новый. `stop` отличает «декодер сам кончился» от «обои сменили»: во
    /// втором случае перезапускать нечего.
    child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>,
    stop: std::sync::Arc<AtomicBool>,
}

impl Drop for Anim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// Умеет ли ffmpeg на этой машине декодировать видеокартой (NVDEC через CUDA).
///
/// Проверяем ОДИН раз и самым дешёвым способом — созданием hw-устройства, без
/// файла и без декодирования. Проверка обязательна: `-hwaccel cuda` НЕ
/// откатывается на программный декодер сам — если устройства нет, ffmpeg
/// печатает «No device available for decoder» и не отдаёт ни одного кадра
/// (проверено 05.08.2026), то есть обои просто не появились бы.
fn cuda_available() -> bool {
    static ОК: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ОК.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .args([
                "-loglevel", "error", "-init_hw_device", "cuda",
                "-f", "lavfi", "-i", "nullsrc", "-frames:v", "1", "-f", "null", "-",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Запустить ffmpeg на живые обои. `hw` — декодировать видеокартой.
fn spawn_ffmpeg(path: &str, w: u32, h: u32, fps: u64, hw: bool) -> Option<std::process::Child> {
    let mut args: Vec<String> = vec![
        "-loglevel".into(), "error".into(), "-nostdin".into(),
    ];
    if hw {
        args.push("-hwaccel".into());
        args.push("cuda".into());
    }
    args.extend([
        "-stream_loop".to_string(), "-1".into(), "-i".into(), path.to_string(),
        "-an".into(), "-sn".into(),
        // ВАЖЕН ПОРЯДОК: `fps` стоит ПЕРВЫМ в цепочке фильтров, до scale.
        // Раньше частоту резал выходной ключ `-r 15`, то есть уже ПОСЛЕ
        // фильтров: масштабирование 4K→2560×1080 честно считалось на всех 60
        // кадрах источника, а три из четырёх результатов выбрасывались.
        // Замер на green-tree (3840×2160@60): 36.9 с CPU на 12 с видео против
        // 27.9 с после переноса fps в начало (−24%).
        "-vf".into(),
        format!("fps={fps},scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"),
        // bgra, а не rgba: ровно то, что ждёт wl_shm — попиксельный
        // своп на 2.7 млн пикселей за кадр просто исчезает.
        "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "bgra".into(), "-".into(),
    ]);
    std::process::Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("dwall: ffmpeg не запустился: {e}"))
        .ok()
}

/// Видео ли это (для них кадры гонит ffmpeg).
fn is_video(path: &str) -> bool {
    matches!(
        std::path::Path::new(path).extension().and_then(|e| e.to_str())
            .unwrap_or("").to_lowercase().as_str(),
        "mp4" | "webm" | "mkv" | "mov" | "avi"
    )
}

/// Запускает фоновое декодирование живых обоев под размер экрана `w`×`h`.
fn start_anim(path: &str, w: u32, h: u32) -> Option<Anim> {
    use std::sync::mpsc::sync_channel;
    let (tx, rx) = sync_channel::<(u32, u32, Vec<u8>, u64, bool)>(1);

    if is_video(path) {
        // Кадры просим сразу в размер экрана и в BGRA — масштабировать и
        // конвертировать на нашей стороне было бы втрое дороже.
        // -stream_loop -1 — бесконечный повтор, fps=15 — потолок частоты:
        // обои не стоят 60 к/с процессорного времени.
        //
        // Декодируем видеокартой, когда она есть: 4K@60 h264 — это ~28 с CPU на
        // каждые 12 с видео (замер 05.08.2026, зелёное дерево 3840×2160),
        // против 5.7 с через NVDEC. Именно этот декодер и держал на машине
        // Ярика ~200% CPU в фоне при неподвижных обоях.
        const FPS: u64 = 15;
        let hw = cuda_available();
        let child = std::sync::Arc::new(std::sync::Mutex::new(Some(
            spawn_ffmpeg(path, w, h, FPS, hw)?,
        )));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (поток_child, поток_stop) = (child.clone(), stop.clone());
        let путь = path.to_string();
        std::thread::spawn(move || {
            use std::io::Read;
            let кадр = (w as usize) * (h as usize) * 4;
            let mut buf = vec![0u8; кадр];
            let mut hw = hw;
            loop {
                // Забираем stdout только что запущенного процесса.
                let out = поток_child.lock().ok().and_then(|mut g| {
                    g.as_mut().and_then(|c| c.stdout.take())
                });
                let Some(mut out) = out else { return };

                let mut кадров = 0u64;
                loop {
                    if out.read_exact(&mut buf).is_err() {
                        break; // ffmpeg закончился или его убили
                    }
                    кадров += 1;
                    // Свежий буфер вместо clone: read_exact всё равно перезапишет
                    // его целиком, а копия 11 МБ на кадр — это 165 МБ/с memcpy
                    // впустую.
                    let готов = std::mem::replace(&mut buf, vec![0u8; кадр]);
                    // Ошибка отправки = главный цикл выбросил Anim, уходим.
                    if tx.send((w, h, готов, 1000 / FPS, true)).is_err() {
                        return;
                    }
                }

                if поток_stop.load(Ordering::SeqCst) {
                    return; // обои сменили — перезапускать нечего
                }
                // Аппаратный декодер не отдал НИ ОДНОГО кадра: устройство есть,
                // но этот кодек ему не по зубам (AV1 на старой карте, редкий
                // профиль). Молча повторяем программным — обои важнее экономии.
                if hw && кадров == 0 {
                    hw = false;
                    eprintln!("dwall: аппаратный декодер не справился, повтор программным");
                    let Some(новый) = spawn_ffmpeg(&путь, w, h, FPS, false) else { return };
                    match поток_child.lock() {
                        Ok(mut g) => *g = Some(новый),
                        Err(_) => return,
                    }
                    continue;
                }
                return;
            }
        });
        return Some(Anim { rx, next_at: std::time::Instant::now(), child, stop });
    }

    // ── GIF ──────────────────────────────────────────────────────────────────
    //
    // Кадр в gif — это НЕ картинка целиком, а заплатка: свой прямоугольник
    // (left, top, width, height), своя прозрачность и правило, что делать с
    // предыдущим кадром (dispose). Раньше dwall брал буфер кадра как есть и
    // растягивал его на весь экран, игнорируя и смещение, и прозрачность.
    // Итог ровно тот, на который жаловались: у живых обоев поверх картинки
    // мигали прямоугольные куски — то чёрные (там, где в заплатке
    // прозрачность), то не на своём месте (заплатку в пол-экрана тянуло на
    // весь экран). Проверено 04.08.2026: `test_anim.gif` показывался красной
    // кашей с радужной полосой снизу.
    //
    // Теперь собираем кадры как положено: держим ХОЛСТ в логический размер
    // gif, накладываем на него каждую заплатку, отдаём наружу снимок холста и
    // применяем dispose. Кадры не копим — декодер идёт потоком (канал ёмкостью
    // 1 его тормозит), а на повтор просто перечитываем файл: иначе гиф в
    // 1080p на сотню кадров занял бы гигабайт.
    let path = path.to_string();

    // Проба: анимация ли это вообще. Одиночный кадр — обычная картинка, и
    // показывать её должен статичный путь (иначе мы зря держим поток).
    {
        let file = std::fs::File::open(&path).ok()?;
        let mut d = gif::DecodeOptions::new();
        d.set_color_output(gif::ColorOutput::RGBA);
        let mut r = d.read_info(file).ok()?;
        let mut n = 0;
        while n < 2 {
            match r.read_next_frame() {
                Ok(Some(_)) => n += 1,
                _ => break,
            }
        }
        if n < 2 { return None; }
    }

    std::thread::spawn(move || {
        loop {
            let Ok(file) = std::fs::File::open(&path) else { return };
            let mut d = gif::DecodeOptions::new();
            d.set_color_output(gif::ColorOutput::RGBA);
            let Ok(mut r) = d.read_info(file) else { return };
            let (sw, sh) = (r.width() as usize, r.height() as usize);
            if sw == 0 || sh == 0 { return; }
            // Холст живёт от кадра к кадру: заплатки накладываются на него.
            let mut холст = vec![0u8; sw * sh * 4];
            loop {
                let f = match r.read_next_frame() {
                    Ok(Some(f)) => f,
                    _ => break,
                };
                let (fx, fy) = (f.left as usize, f.top as usize);
                let (fw, fh) = (f.width as usize, f.height as usize);
                // dispose = Previous: после показа холст обязан вернуться к
                // тому, что было под заплаткой, — сохраняем этот кусок.
                let сохранённое = (f.dispose == gif::DisposalMethod::Previous)
                    .then(|| вырезать(&холст, sw, fx, fy, fw, fh));

                // Накладываем заплатку. Прозрачные пиксели (alpha 0) НЕ
                // копируем — под ними должен остаться прежний кадр.
                for y in 0..fh {
                    let dy = fy + y;
                    if dy >= sh { break; }
                    for x in 0..fw {
                        let dx = fx + x;
                        if dx >= sw { break; }
                        let s = (y * fw + x) * 4;
                        if s + 3 >= f.buffer.len() || f.buffer[s + 3] == 0 { continue; }
                        let d = (dy * sw + dx) * 4;
                        холст[d..d + 4].copy_from_slice(&f.buffer[s..s + 4]);
                    }
                }

                // Наружу отдаём снимок холста: BGRA (как ждёт wl_shm) и
                // непрозрачный — обои не должны просвечивать до чёрного фона.
                let mut кадр = холст.clone();
                for c in кадр.chunks_exact_mut(4) { c.swap(0, 2); c[3] = 255; }
                // delay в gif — сотые доли секунды; 0 трактуем как 100 мс
                // (так же поступают браузеры).
                let ms = if f.delay == 0 { 100 } else { f.delay as u64 * 10 };
                if tx.send((sw as u32, sh as u32, кадр, ms, true)).is_err() {
                    return; // главный цикл выбросил Anim
                }

                match f.dispose {
                    gif::DisposalMethod::Background => {
                        // Участок кадра гасим в прозрачность.
                        for y in 0..fh {
                            let dy = fy + y;
                            if dy >= sh { break; }
                            let от = (dy * sw + fx.min(sw)) * 4;
                            let до = (dy * sw + (fx + fw).min(sw)) * 4;
                            холст[от..до].fill(0);
                        }
                    }
                    gif::DisposalMethod::Previous => {
                        if let Some(кусок) = сохранённое {
                            вернуть(&mut холст, sw, fx, fy, fw, fh, &кусок);
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    // У gif своего процесса нет — убивать в Drop нечего.
    Some(Anim {
        rx,
        next_at: std::time::Instant::now(),
        child: std::sync::Arc::new(std::sync::Mutex::new(None)),
        stop: std::sync::Arc::new(AtomicBool::new(false)),
    })
}

/// Копия прямоугольника холста (для gif-dispose = Previous).
fn вырезать(холст: &[u8], sw: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let dy = y + row;
        let от = (dy * sw + x) * 4;
        let до = от + w * 4;
        if до <= холст.len() {
            out.extend_from_slice(&холст[от..до]);
        } else {
            out.resize(out.len() + w * 4, 0);
        }
    }
    out
}

/// Вернуть на место кусок, снятый [`вырезать`].
fn вернуть(холст: &mut [u8], sw: usize, x: usize, y: usize, w: usize, h: usize, кусок: &[u8]) {
    for row in 0..h {
        let dy = y + row;
        let от = (dy * sw + x) * 4;
        let до = от + w * 4;
        let s = row * w * 4;
        if до <= холст.len() && s + w * 4 <= кусок.len() {
            холст[от..до].copy_from_slice(&кусок[s..s + w * 4]);
        }
    }
}

// ── Состояние ───────────────────────────────────────────────────────────────

struct Config {
    outputs: Vec<(WlOutput, u32, u32)>,
    bg_surface: Option<WlSurface>,
    bg_layer: Option<ZwlrLayerSurfaceV1>,
    ol_surface: Option<WlSurface>,
    ol_layer: Option<ZwlrLayerSurfaceV1>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    pointer: Option<WlPointer>,
    qh: Option<QueueHandle<Config>>,
    signalfd: Option<i32>,
    wallpapers: Vec<String>,
    current: usize,
    img: Option<(u32, u32, Vec<u8>)>,
    thumbnails: Vec<(u32, u32, Vec<u8>)>,
    /// Для какого списка обоев посчитаны thumbnails — чтобы не декодировать
    /// картинки заново на каждое открытие меню (см. load_thumbnails).
    thumbs_for: Vec<String>,
    /// Обои декодируются в ОТДЕЛЬНОМ потоке: 4K-PNG разбирается сотни
    /// миллисекунд, и синхронный вызов подвешивал весь цикл — меню замирало,
    /// а обои «применялись с задержкой». Здесь ждём результат.
    pending_img: Option<std::sync::mpsc::Receiver<Option<(u32, u32, Vec<u8>)>>>,
    /// Миниатюры тоже считаются в фоне: три 4K-PNG декодировались секундами, и
    /// всё это время меню просто не появлялось на экране.
    pending_thumbs: Option<std::sync::mpsc::Receiver<Vec<(u32, u32, Vec<u8>)>>>,
    /// Живые обои: mp4/webm/mkv через ffmpeg и анимированный gif своим
    /// декодером. Кадры приходят из фонового потока по одному.
    anim: Option<Anim>,
    /// Кадр уже в BGRA (как ждёт wl_shm) — своп не нужен. Так приходят кадры
    /// видео (ffmpeg отдаёт bgra) и gif (свопнуты один раз при декодировании).
    img_bgra: bool,
    /// Два постоянных буфера обоев: пишем по очереди в тот, что не показан.
    /// Раньше на КАЖДЫЙ кадр создавались memfd + mmap + пул + wl_buffer —
    /// именно это, вместе с Lanczos, и съедало треть ядра.
    bg_slots: Vec<ShmSlot>,
    bg_slot: usize,
    bg_dims: (u32, u32),
    /// Такая же постоянная пара под меню. Раньше на КАЖДУЮ перерисовку
    /// (а она шла на каждое движение мыши) заводились новые memfd + пул +
    /// wl_buffer, и никто их не уничтожал: `drop` у прокси wayland-client
    /// ничего не шлёт серверу. Утекало не у нас, а у КОМПОЗИТОРА — он держал
    /// по дескриптору и по 1.4 МБ на каждую, упирался в лимит 1024 fd и рвал
    /// соединение. Отсюда и «обои пропали, меню не открывается».
    ol_slots: Vec<ShmSlot>,
    ol_slot: usize,
    /// Кадр обоев не удалось отдать (оба буфера ещё у композитора) — повторить.
    bg_pending: bool,
    /// Карточка под курсором. Перерисовываем меню только когда она сменилась —
    /// иначе одно движение мыши через меню давало сотню перерисовок.
    hover: Option<usize>,
    /// Что показать при старте. Запускается не сразу: живым обоям нужен размер
    /// экрана, а он известен только после первого wl_output::Mode.
    стартовые: Option<String>,
    overlay_visible: bool,
    cursor_x: f64,
    cursor_y: f64,
    hit_rects: Vec<HitRect>,
    dirty: bool,
    /// Идущий сейчас переход между обоями (см. [`Fade`]).
    fade: Option<Fade>,
    /// Статичные обои, уже растянутые под выход и переведённые в BGRA.
    ///
    /// Нужен из-за перехода: он перерисовывает кадр десятки раз, а Lanczos на
    /// 4K стоит сотни миллисекунд — без кэша «плавная» смена превратилась бы
    /// в слайд-шоу из трёх кадров. Сбрасывается вместе с `img`.
    scaled: Option<Vec<u8>>,
    /// Что показывает сетка: свои обои или выдачу из сети.
    вкладка: Вкладка,
    /// Найденное в сети и разобранные миниатюры к нему (индексы совпадают).
    находки: Vec<online::Находка>,
    сетевые_миниатюры: Vec<(u32, u32, Vec<u8>)>,
    /// Что ищем. Пусто — показываем то, что источник считает лучшим.
    запрос: String,
    /// Какая страница выдачи показана. Прокрутка до низа подгружает следующую.
    сетевая_страница: u32,
    /// Идёт запрос в сеть. Второй такой же не начинаем: сеть медленная, а
    /// щёлкать по вкладкам можно быстро.
    сетевая_работа: bool,
    /// Последняя беда с сетью — рисуем полоской под вкладками. Текста в меню
    /// нет (шрифта у dwall тоже), поэтому объясняемся цветом.
    сетевая_ошибка: bool,
    pending_online: Option<std::sync::mpsc::Receiver<ИзСети>>,
    /// Первый видимый ряд сетки обоев.
    scroll_row: u32,
    /// Накопитель колеса: одна «щёлка» приходит долями, шаг делаем по сумме.
    scroll_acc: f64,
    /// Пришёл ли первый configure для фоновой поверхности.
    ///
    /// wlr-layer-shell запрещает attach до ack первого configure — иначе
    /// композитор рвёт соединение с «must ack the initial configure before
    /// attaching buffer». Раньше рендер запускался прямо из wl_output.Mode,
    /// то есть ДО конфигурации, и dwall падал на старте.
    bg_configured: bool,
    ol_configured: bool,
}

// ── Главная функция ─────────────────────────────────────────────────────────

fn main() {
    // Служебный режим: пересчитать палитру по обоям и разойтись. Композитор
    // для этого не нужен, поэтому проверяется он отдельно от всего остального.
    if std::env::args().nth(1).as_deref() == Some("--palette") {
        std::process::exit(palette::режим_палитры(std::env::args().nth(2)));
    }

    // Служебный режим: спросить обои у источника и показать, что нашлось.
    // Композитор не нужен — сеть проверяется отдельно от всего остального.
    if std::env::args().nth(1).as_deref() == Some("--online") {
        std::process::exit(режим_сети(
            std::env::args().nth(2),
            std::env::args().nth(3),
            std::env::args().nth(4),
        ));
    }

    let conn = Connection::connect_to_env().unwrap();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    // Блокируем сигналы и создаём signalfd
    let sfd = setup_signalfd();

    let mut config = Config {
        outputs: Vec::new(),
        bg_surface: None,
        bg_layer: None,
        ol_surface: None,
        ol_layer: None,
        compositor: None,
        shm: None,
        layer_shell: None,
        pointer: None,
        qh: Some(qh.clone()),
        signalfd: Some(sfd),
        wallpapers: Vec::new(),
        current: 0,
        img: None,
        thumbnails: Vec::new(),
        thumbs_for: Vec::new(),
        pending_img: None,
        pending_thumbs: None,
        anim: None,
        img_bgra: false,
        bg_slots: Vec::new(),
        bg_slot: 0,
        bg_dims: (0, 0),
        ol_slots: Vec::new(),
        ol_slot: 0,
        bg_pending: false,
        hover: None,
        стартовые: None,
        overlay_visible: false,
        cursor_x: -1.0,
        cursor_y: -1.0,
        hit_rects: Vec::new(),
        dirty: false,
        fade: None,
        scaled: None,
        вкладка: Вкладка::Мои,
        находки: Vec::new(),
        сетевые_миниатюры: Vec::new(),
        запрос: String::new(),
        сетевая_страница: 1,
        сетевая_работа: false,
        сетевая_ошибка: false,
        pending_online: None,
        scroll_row: 0,
        scroll_acc: 0.0,
        bg_configured: false,
        ol_configured: false,
    };

    load_config(&mut config);

    if let Some(path) = std::env::args().nth(1) {
        if !config.wallpapers.contains(&path) {
            config.wallpapers.push(path.clone());
            save_config(&config.wallpapers);
        }
        config.current = config.wallpapers.iter().position(|p| p == &path).unwrap_or(0);
        // Через ту же дверь, что и выбор в меню: иначе `dwall видео.mp4`
        // показывал бы один кадр вместо живых обоев.
        config.стартовые = Some(config.wallpapers[config.current].clone());
    } else if let Some(запомненные) = load_current()
        .filter(|p| config.wallpapers.contains(p))
    {
        // Восстанавливаем последние показанные обои: без этого при каждом
        // перезаходе сессия начиналась с первой строки списка.
        config.current = config.wallpapers.iter().position(|p| p == &запомненные).unwrap_or(0);
        config.стартовые = Some(запомненные);
    } else if let Some(first) = config.wallpapers.first() {
        config.стартовые = Some(first.clone());
    }

    // Первый раундтрип — получаем все глобалы
    event_queue.roundtrip(&mut config).unwrap();

    // Главный poll-цикл
    loop {
        // Сначала флашим и диспатчим то что есть.
        //
        // Ошибку dispatch_pending раньше глушили через `let _`, и это стоило
        // дорого: если очередь не разбирается (протокольная ошибка, мёртвое
        // соединение), она остаётся непустой, prepare_read НАВСЕГДА возвращает
        // None, цикл перестаёт спать на poll и крутится на 100% CPU. Хуже
        // того, signalfd читается только в ветке с poll — то есть USR1/USR2
        // в таком состоянии не доходят вообще: обои и меню «залипали» после
        // первого же открытия. Теперь выходим: обои перезапустит сторож
        // в launch_native.sh.
        // Ошибку flush тоже нельзя глушить: после смерти композитора запись в
        // сокет даёт EPIPE, и без выхода мы просто крутились бы дальше.
        if let Err(e) = event_queue.flush() {
            if !временная_ошибка(&e) {
                eprintln!("dwall: соединение с композитором оборвалось ({e}) — выходим");
                return;
            }
        }
        if let Err(e) = event_queue.dispatch_pending(&mut config) {
            eprintln!("dwall: очередь событий сломалась ({e}) — выходим");
            return;
        }

        // Пробуем приготовиться к чтению wayland
        match event_queue.prepare_read() {
            Some(guard) => {
                let wl_fd = event_queue.as_fd().as_raw_fd();
                let mut fds = [
                    libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: sfd, events: libc::POLLIN, revents: 0 },
                ];

                // Ждём события на любом из FD. Таймаут (а не -1) — страховка:
                // даже если мы прозевали пробуждение, цикл раз в секунду
                // очнётся сам и перечитает сигналы.
                // Пока обои декодируются в фоне, опрашиваем чаще, чтобы
                // применить их сразу по готовности, а не через секунду.
                let идёт_переход = config.fade.as_ref().map(|f| !f.ждём).unwrap_or(false);
                let таймаут = if config.anim.is_some() || идёт_переход {
                // Живые обои и переход: опрашиваем часто, иначе кадры идут
                // рывками.
                15
            } else if config.pending_img.is_some() || config.pending_thumbs.is_some() {
                30
            } else {
                1000
            };
                unsafe { libc::poll(fds.as_mut_ptr(), 2, таймаут); }

                // Wayland
                //
                // Обрыв соединения обязан УБИВАТЬ dwall, а не игнорироваться.
                // Раньше здесь стояло `guard.read().unwrap_or(0)`: когда
                // композитор закрывал соединение (например упёршись в лимит
                // дескрипторов), ошибка молча проглатывалась, мёртвый сокет
                // навсегда оставался в POLLHUP, poll возвращался мгновенно —
                // и dwall крутился на 100% ядра, живой, но без обоев и без
                // меню. Сторож в launch_native.sh перезапускает только
                // ЗАВЕРШИВШИЙСЯ процесс, поэтому обои не возвращались до конца
                // сессии. Замер 04.08.2026: 94% CPU, «Connection reset by peer»
                // в stderr, экран пустой.
                if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    eprintln!("dwall: композитор закрыл соединение — выходим");
                    return;
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    if let Err(e) = guard.read() {
                        if !временная_ошибка(&e) {
                            eprintln!("dwall: чтение из сокета не удалось ({e}) — выходим");
                            return;
                        }
                    }
                    if let Err(e) = event_queue.dispatch_pending(&mut config) {
                        eprintln!("dwall: очередь событий сломалась ({e}) — выходим");
                        return;
                    }
                }

                // Сигналы
                if fds[1].revents & libc::POLLIN != 0 {
                    process_signalfd(&mut config);
                }
            }
            None => {
                // Есть неразобранные события — их разберёт dispatch_pending в
                // начале следующего прохода. Сигналы читаем и здесь: иначе в
                // этой ветке signalfd не опрашивается никем, и нажатия Win+W
                // теряются ровно до тех пор, пока не придёт событие Wayland.
                process_signalfd(&mut config);
            }
        }

        // Пост-обработка сигналов и dirty-флагов
        if SIG1.swap(false, Ordering::SeqCst) && !config.wallpapers.is_empty() {
            config.current = (config.current + 1) % config.wallpapers.len();
            let путь = config.wallpapers[config.current].clone();
            start_load_wallpaper(&mut config, путь);
        }
        if SIG2.swap(false, Ordering::SeqCst) {
            config.overlay_visible = !config.overlay_visible;
            if config.overlay_visible {
                load_thumbnails(&mut config);
                build_ui_layout(&mut config);
            }
            config.dirty = true;
        }
        // Стартовые обои запускаем, как только стал известен размер экрана.
        if config.стартовые.is_some()
            && config.outputs.first().map(|(_, w, h)| *w > 0 && *h > 0).unwrap_or(false)
        {
            if let Some(путь) = config.стартовые.take() {
                start_load_wallpaper(&mut config, путь);
            }
        }

        // Живые обои: забираем очередной кадр, когда подошёл его срок.
        if let Some(anim) = config.anim.as_mut() {
            let сейчас = std::time::Instant::now();
            if сейчас >= anim.next_at {
                match anim.rx.try_recv() {
                    Ok((w, h, px, ms, bgra)) => {
                        anim.next_at = сейчас + std::time::Duration::from_millis(ms.max(20));
                        config.img_bgra = bgra;
                        config.img = Some((w, h, px));
                        // Первый кадр новых живых обоев — с него и начинается
                        // переход.
                        if let Some(f) = config.fade.as_mut() { f.ждём = false; }
                        config.scaled = None;
                        render_wallpaper(&mut config);
                    }
                    // Декодер умер (ffmpeg не смог, файл битый) — перестаём ждать.
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => config.anim = None,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
        }

        // Переход между статичными обоями: кадры гоним сами. У живых обоев
        // этим занимается ветка выше — там кадр и так приходит десятки раз в
        // секунду, и смешивание делается заодно с ним.
        if config.anim.is_none() && config.fade.as_ref().map(|f| !f.ждём).unwrap_or(false) {
            render_wallpaper(&mut config);
        }

        // Ответ из сети.
        if let Some(rx) = config.pending_online.as_ref() {
            match rx.try_recv() {
                Ok(ответ) => {
                    config.pending_online = None;
                    config.сетевая_работа = false;
                    match ответ {
                        ИзСети::Список(находки, мини) => {
                            config.находки.extend(находки);
                            config.сетевые_миниатюры.extend(мини);
                        }
                        ИзСети::Скачано(путь) => {
                            // Скачанное добавляем в свой список и сразу ставим:
                            // человек за этим и щёлкал.
                            let путь = путь.to_string_lossy().to_string();
                            if !config.wallpapers.contains(&путь) {
                                config.wallpapers.push(путь.clone());
                                save_config(&config.wallpapers);
                                config.thumbs_for.clear();
                                config.thumbnails.clear();
                            }
                            config.current = config
                                .wallpapers
                                .iter()
                                .position(|p| p == &путь)
                                .unwrap_or(0);
                            start_load_wallpaper(&mut config, путь);
                            // Возвращаемся к своим обоям: новые уже там, и
                            // видно, что они встали.
                            config.вкладка = Вкладка::Мои;
                            config.scroll_row = 0;
                            load_thumbnails(&mut config);
                        }
                        ИзСети::Ошибка(e) => {
                            eprintln!("dwall: сеть — {e}");
                            config.сетевая_ошибка = true;
                        }
                    }
                    build_ui_layout(&mut config);
                    config.dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    config.pending_online = None;
                    config.сетевая_работа = false;
                    config.сетевая_ошибка = true;
                    config.dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Готовы ли миниатюры?
        if let Some(rx) = config.pending_thumbs.as_ref() {
            match rx.try_recv() {
                Ok(t) => {
                    config.pending_thumbs = None;
                    config.thumbnails = t;
                    if config.overlay_visible {
                        config.dirty = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => config.pending_thumbs = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Готовы ли обои из фонового потока?
        if let Some(rx) = config.pending_img.as_ref() {
            match rx.try_recv() {
                Ok(img) => {
                    config.pending_img = None;
                    config.img_bgra = false; // из load_image приходит RGBA
                    config.img = img;
                    // Картинка сменилась — старый растянутый кадр недействителен,
                    // и с этого момента идут часы перехода.
                    config.scaled = None;
                    if let Some(f) = config.fade.as_mut() { f.ждём = false; }
                    render_wallpaper(&mut config);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => config.pending_img = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Поверхности могли не создаться на первом wl_output::Mode: dwall
        // стартует из launch_native.sh сразу, как появился сокет, то есть
        // ПОСРЕДИ инициализации композитора, и Mode приходит раньше глобала
        // zwlr_layer_shell_v1. setup_layers тогда молча выходит, а второго
        // Mode уже не будет — обои и меню не появлялись до конца сессии.
        // Пробуем каждый проход: функция идемпотентна (создаёт поверхности
        // только когда bg_surface пуст) и сама выходит, пока чего-то нет.
        if config.bg_surface.is_none() {
            setup_layers(&mut config);
        }
        // Кадр обоев, который не влез (оба буфера были заняты композитором).
        if config.bg_pending {
            render_wallpaper(&mut config);
        }
        if config.dirty {
            // Пометку снимаем только если кадр действительно ушёл: иначе
            // (оба буфера ещё у композитора) меню осталось бы со старой
            // картинкой до следующего события.
            let ушло = if config.overlay_visible {
                render_overlay(&mut config)
            } else {
                hide_overlay(&mut config)
            };
            config.dirty = !ушло;
        }
    }
}

/// `dwall --online картинки|живые [запрос] [скачать]` — проверить источник.
///
/// Без композитора: показывает, что нашлось на первой странице, а со словом
/// «скачать» четвёртым доводом ещё и тянет первую находку. Нужен для проверки —
/// сеть отказывает иначе, чем графика, и мешать эти два вида отказов в одном
/// месте не стоит.
fn режим_сети(какие: Option<String>, запрос: Option<String>, действие: Option<String>) -> i32 {
    let источник = match какие.as_deref() {
        Some("картинки") | Some("images") => online::Источник::Картинки,
        Some("живые") | Some("live") => online::Источник::Живые,
        _ => {
            eprintln!("dwall --online картинки|живые [запрос] [скачать]");
            return 2;
        }
    };

    let запрос = запрос.unwrap_or_default();
    let находки = match online::список(источник, 1, 2560, 1080, &запрос) {
        Ok(н) => н,
        Err(e) => {
            eprintln!("не вышло: {e}");
            return 1;
        }
    };

    println!("найдено: {}{}", находки.len(),
             if запрос.is_empty() { String::new() } else { format!(" по запросу «{запрос}»") });
    for н in находки.iter().take(5) {
        println!("  {} — {}", н.имя, н.миниатюра);
    }

    if действие.as_deref() == Some("скачать") {
        let Some(первая) = находки.first() else { return 1 };
        match online::скачать(первая, &каталог_загрузок()) {
            Ok(путь) => {
                let размер = std::fs::metadata(&путь).map(|m| m.len()).unwrap_or(0);
                println!("скачано: {} ({} байт)", путь.display(), размер);
            }
            Err(e) => {
                eprintln!("скачать не вышло: {e}");
                return 1;
            }
        }
    }
    0
}

/// Ошибка, после которой можно продолжать (сокет просто пуст или занят).
/// Всё остальное — обрыв связи с композитором, и жить дальше нам незачем.
fn временная_ошибка(e: &wayland_client::backend::WaylandError) -> bool {
    match e {
        wayland_client::backend::WaylandError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
        _ => false,
    }
}

// ── Signal FD ────────────────────────────────────────────────────────────────

fn setup_signalfd() -> i32 {
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        // Блокируем USR1 и USR2 — они пойдут через signalfd
        libc::sigaddset(&mut mask, libc::SIGUSR1);
        libc::sigaddset(&mut mask, libc::SIGUSR2);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if fd < 0 { panic!("signalfd failed"); }
    fd
}

fn process_signalfd(config: &mut Config) {
    let sfd = match config.signalfd { Some(fd) => fd, None => return };
    let mut buf: [u8; 128] = [0; 128]; // sizeof(signalfd_siginfo)
    loop {
        let n = unsafe { libc::read(sfd, buf.as_mut_ptr() as *mut libc::c_void, 128) };
        if n <= 0 { break; }
        if n >= 4 {
            // signalfd_siginfo: первые 4 байта — ssi_signo (i32)
            let signo = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
            match signo {
                libc::SIGUSR1 => SIG1.store(true, Ordering::SeqCst),
                libc::SIGUSR2 => SIG2.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
    }
}

static SIG1: AtomicBool = AtomicBool::new(false);
static SIG2: AtomicBool = AtomicBool::new(false);

// ── Wayland Dispatch ─────────────────────────────────────────────────────────

impl Dispatch<WlRegistry, ()> for Config {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        if let Event::Global { name, ref interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => state.compositor = Some(registry.bind(name, version, qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, version, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_output" => {
                    let output: WlOutput = registry.bind(name, version, qh, ());
                    state.outputs.push((output, 0, 0));
                }
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for Config {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { flags: _, width, height, refresh: _ } = event {
            for (ref o, w, h) in &mut state.outputs {
                if o.id() == output.id() { *w = width as u32; *h = height as u32; }
            }
            // Только создаём поверхности. Рисовать отсюда нельзя: первый
            // configure ещё не пришёл, а attach до его ack — протокольная
            // ошибка. Рендер запускает сам обработчик Configure.
            if state.bg_surface.is_none() { setup_layers(state); }
            if state.bg_configured { render_wallpaper(state); }
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Config {
    fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: zwlr_layer_shell_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Config {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surface.ack_configure(serial);
                let id = surface.id();
                let is_bg = state.bg_layer.as_ref().map(|l| l.id()) == Some(id.clone());
                let is_ol = state.ol_layer.as_ref().map(|l| l.id()) == Some(id);
                if is_bg {
                    // Размер обоев берём из configure, а не из wl_output.Mode:
                    // Mode отдаёт физические пиксели режима, а layer-shell —
                    // логический размер, который композитор реально отвёл. При
                    // scale != 1 это разные числа, и буфер разъезжался бы с экраном.
                    let mut changed = !state.bg_configured;
                    if width > 0 && height > 0 {
                        if let Some((_, w, h)) = state.outputs.first_mut() {
                            if *w != width || *h != height {
                                *w = width; *h = height;
                                changed = true;
                            }
                        }
                    }
                    state.bg_configured = true;
                    // Первый ack — рисуем в любом случае: до него мы не имели
                    // права трогать буфер, значит фон ещё пуст.
                    if changed { render_wallpaper(state); }
                } else if is_ol {
                    state.ol_configured = true;
                    if state.overlay_visible { render_overlay(state); }
                }
            }
            // Композитор снёс поверхность (выход отключили, шелл закрылся).
            // Держать на неё ссылку нельзя: следующий commit — протокольная
            // ошибка и дисконнект. Обнуляем, setup_layers создаст заново.
            zwlr_layer_surface_v1::Event::Closed => {
                let id = surface.id();
                if state.bg_layer.as_ref().map(|l| l.id()) == Some(id.clone()) {
                    state.bg_layer = None;
                    state.bg_surface = None;
                    state.bg_configured = false;
                } else if state.ol_layer.as_ref().map(|l| l.id()) == Some(id) {
                    state.ol_layer = None;
                    state.ol_surface = None;
                    state.ol_configured = false;
                    state.overlay_visible = false;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for Config {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlCompositor, ()> for Config {
    fn event(_: &mut Self, _: &WlCompositor, _: <WlCompositor as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for Config {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlPointer, ()> for Config {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        _: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => {}
            wl_pointer::Event::Leave { .. } => {
                state.cursor_x = -1.0; state.cursor_y = -1.0;
                if state.hover.take().is_some() { state.dirty = true; }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.cursor_x = surface_x; state.cursor_y = surface_y;
                // Перерисовываем ТОЛЬКО когда сменилась карточка под курсором:
                // в меню от положения мыши зависит одна подсветка, а событий
                // движения приходит до тысячи в секунду.
                let под_курсором = hover_cell(state);
                if под_курсором != state.hover {
                    state.hover = под_курсором;
                    state.dirty = true;
                }
            }
            // Колесо листает сетку обоев. Композитор шлёт величину в единицах
            // поверхности, одна «щёлка» — около 10..15; копим и шагаем рядами,
            // иначе плавный тачпад давал бы прокрутку на пол-ряда, а её мы не
            // рисуем (см. ui_cells).
            wl_pointer::Event::Axis { axis, value, .. } if state.overlay_visible => {
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    const ЩЁЛЧОК: f64 = 10.0;
                    state.scroll_acc += value;
                    while state.scroll_acc >= ЩЁЛЧОК {
                        state.scroll_acc -= ЩЁЛЧОК;
                        прокрутить(state, 1);
                    }
                    while state.scroll_acc <= -ЩЁЛЧОК {
                        state.scroll_acc += ЩЁЛЧОК;
                        прокрутить(state, -1);
                    }
                }
            }
            wl_pointer::Event::Button { button, state: btn_state, .. }
                if btn_state == WEnum::Value(wl_pointer::ButtonState::Pressed)
                    && state.overlay_visible =>
            {
                // ПКМ по карточке — то же удаление, что и крестиком, но
                // попасть проще: целиться в кнопку 28×28 мышью не всегда
                // удобно, а промах по ней выбирает обои и закрывает меню.
                const BTN_RIGHT: u32 = 0x111;
                let правая = button == BTN_RIGHT;
                // Курсор вне поверхности помечен как -1: `as u32` в Rust
                // насыщает отрицательное к нулю, так что без этой проверки
                // «мышь ушла» нажимало бы карточку в левом верхнем углу.
                if state.cursor_x < 0.0 || state.cursor_y < 0.0 { return; }
                let cx = state.cursor_x as u32;
                let cy = state.cursor_y as u32;
                // Действие вынимаем ДО обработки: тело меняет state (и сам
                // hit_rects через build_ui_layout), а держать на него ссылку
                // в это время нельзя.
                let hit = state.hit_rects.iter().find(|r| {
                    cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h
                }).map(|r| r.action);
                // Для правой кнопки выбор карточки превращаем в удаление.
                let hit = match (правая, hit) {
                    (true, Some(UiAction::SelectWallpaper(i))) => Some(UiAction::DeleteWallpaper(i)),
                    (true, Some(UiAction::AddWallpaper)) => None,
                    (_, h) => h,
                };
                match hit {
                    Some(UiAction::Scroll(шаг)) => прокрутить(state, шаг),
                    Some(UiAction::Вкладка(в)) => открыть_вкладку(state, в),
                    Some(UiAction::Поиск) => {
                        // Меню — слой Overlay, оно рисуется ПОВЕРХ обычных
                        // окон, а fuzzel окно обычное. Не убрав меню, мы бы
                        // спрятали строку ввода под ним — ровно та же беда
                        // была с проводником по «плюсику».
                        state.overlay_visible = false;
                        hide_overlay(state);
                        let _ = conn.flush();

                        let ответ = спросить_запрос(&state.запрос);

                        state.overlay_visible = true;
                        if let Some(новый) = ответ {
                            state.запрос = новый;
                            // Искать по своим обоям нечего — их пять штук и
                            // они все на виду. Поэтому поиск всегда уводит в
                            // сеть: с «моих» переходим на картинки.
                            if state.вкладка == Вкладка::Мои {
                                state.вкладка = Вкладка::Картинки;
                            }
                            state.находки.clear();
                            state.сетевые_миниатюры.clear();
                            state.scroll_row = 0;
                            загрузить_страницу(state, 1, false);
                        }
                        build_ui_layout(state);
                        state.dirty = true;
                    }
                    Some(UiAction::ВзятьИзСети(idx)) => взять_из_сети(state, idx),
                    Some(UiAction::SelectWallpaper(idx)) => {
                        if idx < state.wallpapers.len() {
                            state.current = idx;
                            let путь = state.wallpapers[state.current].clone();
                            start_load_wallpaper(state, путь);
                            state.overlay_visible = false;
                            state.dirty = true;
                        }
                    }
                    Some(UiAction::DeleteWallpaper(idx)) => {
                        if idx < state.wallpapers.len() {
                            state.wallpapers.remove(idx);
                            save_config(&state.wallpapers);
                            // Список поехал — миниатюры пересчитываем, а
                            // указатель на текущие обои поправляем, чтобы он
                            // не показывал на чужую или несуществующую строку.
                            state.thumbs_for.clear();
                            state.thumbnails.clear();
                            if state.wallpapers.is_empty() {
                                state.current = 0;
                                state.img = None;
                                render_wallpaper(state);
                            } else {
                                if state.current >= idx && state.current > 0 {
                                    state.current -= 1;
                                }
                                state.current = state.current.min(state.wallpapers.len() - 1);
                                let путь = state.wallpapers[state.current].clone();
                                start_load_wallpaper(state, путь);
                            }
                            load_thumbnails(state);
                            build_ui_layout(state);
                            state.dirty = true;
                        }
                    }
                    Some(UiAction::AddWallpaper) => {
                        // Меню — это layer-поверхность слоя Overlay, а он по
                        // определению рисуется ПОВЕРХ обычных окон. Проводник
                        // (zenity) — обычное окно, поэтому он открывался ПОД
                        // меню и был не виден. Убираем меню с экрана до запуска
                        // диалога и возвращаем после: выбор происходит поверх
                        // всего, как и ожидается.
                        state.overlay_visible = false;
                        hide_overlay(state);
                        let _ = conn.flush();

                        let выбран = pick_wallpaper_file();

                        // Меню возвращаем в любом случае — и когда файл выбран,
                        // и когда диалог закрыли крестиком.
                        state.overlay_visible = true;
                        // Проверяем, что выбрали именно картинку: иначе в
                        // списке заводились серые плитки-пустышки (диалог
                        // позволяет выбрать что угодно, а декодер потом просто
                        // не справляется).
                        let выбран = выбран.filter(|p| is_image(std::path::Path::new(p)));
                        if let Some(path) = выбран {
                            if !state.wallpapers.contains(&path) {
                                state.wallpapers.push(path.clone());
                                save_config(&state.wallpapers);
                                state.current = state.wallpapers.len() - 1;
                                start_load_wallpaper(state, path.clone());
                                load_thumbnails(state);
                            }
                        }
                        build_ui_layout(state);
                        state.dirty = true;
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for Config {
    fn event(_: &mut Self, _: &WlSurface, _: <WlSurface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for Config {
    fn event(_: &mut Self, _: &WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// Буферы постоянных слотов: ждём `release`, чтобы знать, в какой можно писать.
impl Dispatch<WlBuffer, std::sync::Arc<AtomicBool>> for Config {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        free: &std::sync::Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            free.store(true, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlBuffer, ()> for Config {
    fn event(_: &mut Self, _: &WlBuffer, _: <WlBuffer as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlRegion, ()> for Config {
    fn event(_: &mut Self, _: &WlRegion, _: <WlRegion as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// ── Layer setup ──────────────────────────────────────────────────────────────

fn setup_layers(state: &mut Config) {
    let Some(ref compositor) = state.compositor else { return };
    let Some(ref layer_shell) = state.layer_shell else { return };
    let Some(ref qh) = state.qh else { return };
    let Some((ref output, _, _)) = state.outputs.first() else { return };

    // Background — wallpaper
    let bg = compositor.create_surface(qh, ());
    let bg_layer = layer_shell.get_layer_surface(
        &bg, Some(output), zwlr_layer_shell_v1::Layer::Background,
        "dwall".to_string(), qh, (),
    );
    bg_layer.set_size(0, 0);
    bg_layer.set_anchor(zwlr_layer_surface_v1::Anchor::all());
    bg_layer.set_exclusive_zone(-1);
    bg.commit();
    state.bg_surface = Some(bg);
    state.bg_layer = Some(bg_layer);

    // Overlay — UI выбора.
    //
    // Размер задаём ТОЛЬКО через set_size, без якорей: буфер у нас ровно
    // UI_W×UI_H, а якорь по всем четырём краям заставил бы композитор
    // растянуть поверхность на весь выход, и она разъехалась бы с буфером.
    // Без якорей layer-shell центрирует поверхность на выходе — что нам и надо.
    let ol = compositor.create_surface(qh, ());
    let ol_layer = layer_shell.get_layer_surface(
        &ol, Some(output), zwlr_layer_shell_v1::Layer::Overlay,
        "dwall-overlay".to_string(), qh, (),
    );
    ol_layer.set_size(UI_W, UI_H);
    ol_layer.set_exclusive_zone(-1);
    // Без клавиатуры оверлей нечем закрыть с клавиш — Esc не дойдёт.
    ol_layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
    ol.commit();
    state.ol_surface = Some(ol);
    state.ol_layer = Some(ol_layer);
}

// ── Background rendering ─────────────────────────────────────────────────────

fn render_wallpaper(state: &mut Config) {
    // Пока не подтверждён первый configure, attach запрещён протоколом.
    if !state.bg_configured { return; }
    let (w, h) = match state.outputs.first() {
        Some((_, w, h)) => (*w, *h),
        None => return,
    };
    if w == 0 || h == 0 { return; }
    let buf_size = (w * h * 4) as usize;

    // ── Два постоянных буфера вместо нового на каждый кадр ──────────────────
    // Пишем всегда в тот, что не показан сейчас. Раньше каждый кадр создавал
    // memfd, mmap, пул и wl_buffer — на живых обоях это десятки раз в секунду.
    if state.bg_dims != (w, h) || state.bg_slots.len() < 2 {
        state.bg_slots.clear();
        let (Some(shm), Some(qh)) = (state.shm.as_ref(), state.qh.as_ref()) else { return };
        let Some(slots) = make_slots(shm, qh, w, h) else { return };
        state.bg_slots = slots;
        state.bg_dims = (w, h);
        state.bg_slot = 0;
    }

    // Статичные обои готовим ОДИН раз: растягиваем под выход и переводим в
    // BGRA. Дальше и обычная перерисовка, и каждый кадр перехода — это просто
    // копирование готового буфера.
    //
    // Считаем в локальную переменную, а не прямо в state.scaled: пока живо
    // заимствование state.img, писать в state нельзя.
    let новый_кэш = if state.anim.is_none() && state.scaled.is_none() {
        let bgra = state.img_bgra;
        state.img.as_ref().and_then(|(iw, ih, px)| {
            let mut кадр = if *iw == w && *ih == h && px.len() >= buf_size {
                px[..buf_size].to_vec()
            } else {
                resize(*iw, *ih, px, w, h)
            };
            if !bgra {
                for c in кадр.chunks_exact_mut(4) { c.swap(0, 2); }
            }
            (кадр.len() >= buf_size).then_some(кадр)
        })
    } else {
        None
    };
    if новый_кэш.is_some() {
        state.scaled = новый_кэш;
    }

    // Пишем только в тот буфер, который композитор уже отпустил. Раньше слоты
    // чередовались вслепую, и на живых обоях можно было попасть в буфер,
    // который прямо сейчас читают, — это видно как рваные прямоугольные куски
    // соседнего кадра.
    //
    // Свободного нет — не рисуем, но и не забываем: у живых обоев кадр можно
    // просто потерять, а вот единственный кадр статичной картинки потерять
    // нельзя, иначе обои так и не появятся. Флаг заставит главный цикл
    // повторить попытку.
    let Some(next) = take_free_slot(&state.bg_slots, state.bg_slot) else {
        state.bg_pending = true;
        return;
    };
    state.bg_pending = false;
    state.bg_slot = next;
    let slot = &state.bg_slots[state.bg_slot];
    let dst = unsafe { std::slice::from_raw_parts_mut(slot.ptr, slot.len) };

    match state.img {
        // Статичные обои идут через кэш: во время перехода кадр собирается
        // заново каждые ~16 мс, а Lanczos на 4K столько раз не потянуть.
        Some(_) if state.anim.is_none() && state.scaled.is_some() => {
            let кэш = state.scaled.as_ref().unwrap();
            if кэш.len() >= buf_size {
                dst.copy_from_slice(&кэш[..buf_size]);
            }
        }
        // Кадр уже нужного размера (так отдаёт ffmpeg) — просто копируем.
        // Ни масштабирования, ни свопа: самый дешёвый путь, ради него всё.
        Some((iw, ih, ref px)) if iw == w && ih == h && px.len() >= buf_size => {
            dst.copy_from_slice(&px[..buf_size]);
            if !state.img_bgra {
                for c in dst.chunks_exact_mut(4) { c.swap(0, 2); }
            }
        }
        // Размер не совпал.
        //
        // Для АНИМАЦИИ масштабируем сами, прямо в разделяемую память: крейт
        // image на каждый кадр копировал исходник, заводил ещё один буфер на
        // весь экран и отдавал его нам третьей копией — 33 МБ туда-сюда по
        // десять раз в секунду. Свой блит по ближайшему соседу с заранее
        // посчитанной таблицей столбцов пишет ровно один раз и никаких
        // аллокаций не делает.
        Some((iw, ih, ref px)) if state.anim.is_some() && iw > 0 && ih > 0 => {
            blit_scaled(px, iw, ih, dst, w, h, state.img_bgra);
        }
        // Статичная картинка — путь прежний, качественный: она масштабируется
        // один раз при выборе, экономить тут нечего.
        Some((iw, ih, ref px)) => {
            let mut rgba = resize(iw, ih, px, w, h);
            if !state.img_bgra {
                for c in rgba.chunks_exact_mut(4) { c.swap(0, 2); }
            }
            if rgba.len() < buf_size { return; }
            dst.copy_from_slice(&rgba[..buf_size]);
        }
        None => dst.fill(35),
    }

    // ── Плавный переход ─────────────────────────────────────────────────────
    // В dst уже лежит НОВЫЙ кадр; подмешиваем к нему снимок старого. Смешиваем
    // прямо на месте — лишний буфер на весь экран (это десятки мегабайт) тут
    // ни к чему.
    if let Some(fade) = state.fade.as_mut() {
        if fade.ждём {
            // Новые обои ещё не готовы — показываем старый кадр как есть.
        } else if fade.from.len() >= dst.len() {
            let начало = *fade.start.get_or_insert_with(std::time::Instant::now);
            let прошло = начало.elapsed().as_millis() as u64;
            if прошло >= FADE_MS {
                state.fade = None;
            } else {
                // Сглаживание по краям (smoothstep): линейное появление на
                // глаз начинается рывком и так же обрывается.
                let t = прошло as f32 / FADE_MS as f32;
                let t = t * t * (3.0 - 2.0 * t);
                let новый = (t * 256.0) as u32;
                let старый = 256 - новый;
                for (d, s) in dst.iter_mut().zip(fade.from.iter()) {
                    *d = (((*d as u32) * новый + (*s as u32) * старый) >> 8) as u8;
                }
            }
        } else {
            // Снимок не того размера (экран переключили посреди перехода) —
            // показываем новые обои сразу, без смешивания.
            state.fade = None;
        }
    }

    let Some(surface) = state.bg_surface.as_ref() else { return };
    let slot = &state.bg_slots[state.bg_slot];
    slot.free.store(false, Ordering::Relaxed);
    surface.attach(Some(&slot.buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
}

/// Снять то, что сейчас на экране, — с этого кадра пойдёт переход к новым
/// обоям.
///
/// Берём содержимое ПОКАЗАННОГО буфера, а не исходную картинку: во-первых, он
/// уже растянут под выход и переведён в BGRA (смешивать можно байт в байт),
/// во-вторых, у живых обоев исходного кадра как файла попросту не существует.
fn начать_переход(state: &mut Config) {
    let (w, h) = match state.outputs.first() {
        Some((_, w, h)) => (*w, *h),
        None => return,
    };
    // Первый запуск (буферов ещё нет) или смена разрешения посреди дела —
    // показывать переход не от чего, просто ставим обои.
    if w == 0 || h == 0 || state.bg_slots.is_empty() || state.bg_dims != (w, h) {
        state.fade = None;
        return;
    }
    let Some(slot) = state.bg_slots.get(state.bg_slot) else { return };
    let текущий = unsafe { std::slice::from_raw_parts(slot.ptr, slot.len) }.to_vec();
    state.fade = Some(Fade { from: текущий, start: None, ждём: true });
}

// ── Overlay rendering ────────────────────────────────────────────────────────

/// Готовит (один раз) постоянную пару буферов под меню и возвращает индекс
/// свободного. None — буферов нет или композитор ещё не отпустил ни один.
fn overlay_slot(state: &mut Config) -> Option<usize> {
    if state.ol_slots.len() < 2 {
        let (shm, qh) = (state.shm.as_ref()?, state.qh.as_ref()?);
        state.ol_slots = make_slots(shm, qh, UI_W, UI_H)?;
        state.ol_slot = 0;
    }
    let next = take_free_slot(&state.ol_slots, state.ol_slot)?;
    state.ol_slot = next;
    Some(next)
}

/// Спрятать меню, НЕ размапливая поверхность.
///
/// Раньше здесь стоял `attach(None)` — по протоколу это размап, и после него
/// layer-поверхность возвращается в исходное состояние: следующий буфер
/// прикреплять нельзя, пока не придёт НОВЫЙ configure, а его никто не
/// запрашивал. Итог: меню открывалось ровно один раз, а на втором-третьем
/// круге вместе с ним пропадали и обои. Теперь рисуем полностью прозрачный
/// буфер того же размера и снимаем регион ввода, чтобы невидимое меню не
/// съедало клики по окнам под ним.
fn hide_overlay(state: &mut Config) -> bool {
    let Some(idx) = overlay_slot(state) else { return false };
    let Some(compositor) = state.compositor.clone() else { return false };
    let Some(qh) = state.qh.clone() else { return false };
    let Some(surface) = state.ol_surface.clone() else { return false };

    // Пустой регион ввода: клики проходят насквозь.
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    let slot = &state.ol_slots[idx];
    unsafe { std::slice::from_raw_parts_mut(slot.ptr, slot.len).fill(0) };
    slot.free.store(false, Ordering::Relaxed);
    surface.attach(Some(&slot.buffer), 0, 0);
    surface.damage_buffer(0, 0, UI_W as i32, UI_H as i32);
    surface.commit();
    true
}

/// Рисует меню. `false` — не вышло (нет поверхности или оба буфера ещё у
/// композитора); тогда главный цикл оставляет пометку и повторит.
fn render_overlay(state: &mut Config) -> bool {
    if !state.ol_configured { return false; }
    if state.ol_surface.is_none() || state.shm.is_none()
        || state.qh.is_none() || state.compositor.is_none()
    {
        return false;
    }

    let buf_size = (UI_W * UI_H * 4) as usize;
    let mut pixels = vec![0u8; buf_size];

    // Фон
    fill_rounded_rect(&mut pixels, UI_W, UI_H, 0, 0, UI_W, UI_H, CORNER, BG_COLOR);

    // Вкладки: выбранная светлее, под ней тонкая полоска-подчёркивание.
    for (i, (tx, ty)) in tab_rects().iter().enumerate() {
        let выбрана = ВКЛАДКИ[i] == state.вкладка;
        let фон = if выбрана {
            HOVER_COLOR
        } else if is_hover(state, *tx, *ty, ARROW, ARROW) {
            PLUS_COLOR
        } else {
            CARD_COLOR
        };
        fill_rounded_rect(&mut pixels, UI_W, UI_H, *tx, *ty, ARROW, ARROW, 6, фон);
        let цвет = if выбрана { 0xFFEEEEEEu32 } else { 0xFF888888 };
        draw_tab_icon(&mut pixels, ВКЛАДКИ[i], *tx, *ty, цвет);
        if выбрана {
            fill_rounded_rect(&mut pixels, UI_W, UI_H, *tx, *ty + ARROW + 3, ARROW, 2, 1, SEL_COLOR);
        }
    }

    // Кнопка поиска. Когда запрос задан, она подсвечена — иначе непонятно,
    // почему в сетке не «лучшее», а что-то другое; словами объяснить нечем.
    {
        let (sx, sy) = search_rect();
        let ищем = !state.запрос.is_empty();
        let фон = if ищем {
            HOVER_COLOR
        } else if is_hover(state, sx, sy, ARROW, ARROW) {
            PLUS_COLOR
        } else {
            CARD_COLOR
        };
        fill_rounded_rect(&mut pixels, UI_W, UI_H, sx, sy, ARROW, ARROW, 6, фон);
        draw_search_icon(&mut pixels, sx, sy, if ищем { 0xFFEEEEEEu32 } else { 0xFF888888 });
        if ищем {
            fill_rounded_rect(&mut pixels, UI_W, UI_H, sx, sy + ARROW + 3, ARROW, 2, 1, SEL_COLOR);
        }
    }

    // Полоска состояния сети: серая — идёт запрос, красная — не вышло.
    // Слов в меню нет, поэтому объясняемся цветом.
    if state.вкладка != Вкладка::Мои && (state.сетевая_работа || state.сетевая_ошибка) {
        let (x, y) = tab_rects()[0];
        let ширина = UI_W - 2 * x;
        let цвет = if state.сетевая_ошибка { 0xFFAA4444u32 } else { 0xFF666666 };
        fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y + ARROW + 8, ширина, 2, 1, цвет);
    }

    // Сетевая выдача рисуется своей сеткой: без «+» и без крестиков.
    if state.вкладка != Вкладка::Мои {
        for (idx, x, y) in сетка(state.находки.len(), state.scroll_row) {
            let hover = is_hover(state, x, y, THUMB, THUMB);
            let фон = if hover { HOVER_COLOR } else { CARD_COLOR };
            fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, CORNER, фон);
            if let Some(thumb) = state.сетевые_миниатюры.get(idx) {
                draw_thumbnail(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, thumb);
            }
        }
        нарисовать_прокрутку(&mut pixels, state, предел_прокрутки(state), карточек(state));
        for chunk in pixels.chunks_exact_mut(4) { chunk.swap(0, 2); }
        return отдать_меню(state, pixels, buf_size);
    }

    // Карточки — координаты берём из ui_cells, общих с build_ui_layout.
    let n = state.wallpapers.len();
    let макс = макс_прокрутка(n);

    нарисовать_прокрутку(&mut pixels, state, макс, n + 1);

    for (idx, x, y) in ui_cells(n, state.scroll_row) {
        let hover = is_hover(state, x, y, THUMB, THUMB);
        if idx < n {
            let bg = if hover { HOVER_COLOR } else { CARD_COLOR };
            fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, CORNER, bg);
            if let Some(thumb) = state.thumbnails.get(idx) {
                draw_thumbnail(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, thumb);
            }
            // Индикатор выбранных — слева сверху, чтобы не спорить с крестиком.
            if idx == state.current {
                fill_rounded_rect(&mut pixels, UI_W, UI_H, x + 6, y + 6, 12, 12, 6, SEL_COLOR);
            }
            // Кнопка удаления — показываем только под курсором на карточке,
            // чтобы сетка не пестрела крестиками.
            if hover {
                let dx = x + THUMB - DEL_SIZE - 4;
                let dy = y + 4;
                fill_rounded_rect(&mut pixels, UI_W, UI_H, dx, dy, DEL_SIZE, DEL_SIZE,
                                  DEL_SIZE / 2, DEL_BG);
                draw_cross(&mut pixels, UI_W, UI_H, dx, dy, DEL_SIZE);
            }
        } else {
            let bg = if hover { HOVER_COLOR } else { PLUS_COLOR };
            fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, CORNER, bg);
            draw_plus(&mut pixels, UI_W, UI_H, x, y, THUMB);
        }
    }

    // RGBA → BGRA
    for chunk in pixels.chunks_exact_mut(4) { chunk.swap(0, 2); }
    отдать_меню(state, pixels, buf_size)
}

/// Полоса прокрутки и стрелки. Рисуются одинаково на всех вкладках, поэтому
/// живут отдельно: раньше эта арифметика была вписана прямо в render_overlay,
/// и сетевой сетке пришлось бы её дублировать.
fn нарисовать_прокрутку(pixels: &mut [u8], state: &Config, макс: u32, всего_карточек: usize) {
    if макс == 0 { return; }

    let track_x = UI_W - ARROW - 12 + (ARROW - SB_W) / 2;
    let track_y = MY;
    let track_h = VIS_ROWS * THUMB + (VIS_ROWS - 1) * GAP;
    fill_rounded_rect(pixels, UI_W, UI_H, track_x, track_y,
                      SB_W, track_h, SB_W / 2, SB_COLOR);

    // Бегунок: доля видимой части от всей сетки.
    let всего = рядов(всего_карточек).max(1);
    let высота = (track_h * VIS_ROWS / всего).max(24).min(track_h);
    let свободно = track_h - высота;
    let смещение = свободно * state.scroll_row / макс;
    fill_rounded_rect(pixels, UI_W, UI_H, track_x, track_y + смещение,
                      SB_W, высота, SB_W / 2, SB_THUMB_COLOR);

    let [(ax, ay_up), (_, ay_dn)] = arrow_rects();
    for (ay, вниз, доступна) in
        [(ay_up, false, state.scroll_row > 0), (ay_dn, true, state.scroll_row < макс)]
    {
        let фон = if is_hover(state, ax, ay, ARROW, ARROW) { HOVER_COLOR } else { CARD_COLOR };
        fill_rounded_rect(pixels, UI_W, UI_H, ax, ay, ARROW, ARROW, ARROW / 2, фон);
        // Упёрлись в край — стрелку гасим, чтобы было видно, что дальше
        // листать некуда.
        let цвет = if доступна { 0xFFDDDDDDu32 } else { 0xFF666666 };
        draw_arrow(pixels, UI_W, UI_H, ax, ay, ARROW, вниз, цвет);
    }
}

/// Отдать готовый кадр меню композитору. `false` — оба буфера ещё заняты,
/// главный цикл повторит.
fn отдать_меню(state: &mut Config, pixels: Vec<u8>, buf_size: usize) -> bool {
    let Some(idx) = overlay_slot(state) else { return false };
    let Some(compositor) = state.compositor.clone() else { return false };
    let Some(qh) = state.qh.clone() else { return false };
    let Some(surface) = state.ol_surface.clone() else { return false };

    let slot = &state.ol_slots[idx];
    unsafe { std::slice::from_raw_parts_mut(slot.ptr, slot.len) }
        .copy_from_slice(&pixels[..buf_size]);
    // Меню видимо — возвращаем регион ввода на всю его площадь (при
    // скрытии он снимается, см. hide_overlay).
    let region = compositor.create_region(&qh, ());
    region.add(0, 0, UI_W as i32, UI_H as i32);
    surface.set_input_region(Some(&region));
    region.destroy();
    slot.free.store(false, Ordering::Relaxed);
    surface.attach(Some(&slot.buffer), 0, 0);
    surface.damage_buffer(0, 0, UI_W as i32, UI_H as i32);
    surface.commit();
    true
}

// ── UI helpers ───────────────────────────────────────────────────────────────

/// Какая карточка под курсором (индекс из ui_cells; len() — это «+»).
fn hover_cell(state: &Config) -> Option<usize> {
    if state.cursor_x < 0.0 || state.cursor_y < 0.0 { return None; }
    let cx = state.cursor_x as u32;
    let cy = state.cursor_y as u32;
    сетка(карточек(state), state.scroll_row).into_iter()
        .find(|&(_, x, y)| cx >= x && cx < x + THUMB && cy >= y && cy < y + THUMB)
        .map(|(idx, _, _)| idx)
}

/// Спросить, что искать.
///
/// Своего поля ввода у dwall нет и быть не может: шрифта в нём тоже нет, всё
/// меню рисуется прямоугольниками. Поэтому текст спрашиваем у fuzzel — он уже
/// используется для выбора файла по «плюсику». Ключ `--prompt-only` как раз
/// превращает его в поле свободного ввода: он не ждёт список на входе и
/// печатает то, что набрали.
///
/// `None` — передумали (Esc): прежний запрос остаётся. `Some("")` — стёрли
/// строку и нажали Enter: это значит «показывай снова лучшее».
fn спросить_запрос(текущий: &str) -> Option<String> {
    let bin = which("fuzzel")?;
    let out = std::process::Command::new(bin)
        .args([
            "--dmenu",
            "--prompt-only=Поиск обоев: ",
            "--width=40",
            // Начинаем с прежнего запроса — его чаще правят, чем набирают
            // заново.
            &format!("--search={текущий}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Каталог, куда складываются скачанные обои.
fn каталог_загрузок() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}/.config/dwall/downloaded", home()))
}

/// Переключить вкладку сетки.
fn открыть_вкладку(state: &mut Config, вкладка: Вкладка) {
    if state.вкладка == вкладка { return; }
    state.вкладка = вкладка;
    state.scroll_row = 0;
    state.hover = None;
    state.сетевая_ошибка = false;

    // Выдачу для вкладки грузим один раз за сеанс: повторный заход не должен
    // снова ждать сеть. Обновить — вернуться на «свои» и открыть заново.
    if вкладка != Вкладка::Мои && state.находки.is_empty() && !state.сетевая_работа {
        загрузить_страницу(state, 1, false);
    }

    build_ui_layout(state);
    state.dirty = true;
}

/// Запросить страницу выдачи в фоновом потоке.
///
/// `дополнить` — приклеить к уже показанному (так работает подгрузка при
/// прокрутке вниз), иначе список заменяется целиком.
fn загрузить_страницу(state: &mut Config, страница: u32, дополнить: bool) {
    let Some(источник) = state.вкладка.источник() else { return };
    if state.сетевая_работа { return; }

    let (w, h) = state.outputs.first().map(|(_, w, h)| (*w, *h)).unwrap_or((1920, 1080));
    let (tx, rx) = std::sync::mpsc::channel();
    state.pending_online = Some(rx);
    state.сетевая_работа = true;
    state.сетевая_ошибка = false;
    state.сетевая_страница = страница;
    if !дополнить {
        state.находки.clear();
        state.сетевые_миниатюры.clear();
    }
    state.dirty = true;

    let запрос = state.запрос.clone();
    std::thread::spawn(move || {
        let ответ = match online::список(источник, страница, w, h, &запрос) {
            Err(e) => ИзСети::Ошибка(e),
            Ok(находки) => {
                // Миниатюры качаем и разбираем здесь же: главный цикл получит
                // всё готовым и ни разу не остановится ни на сети, ни на
                // декодировании.
                let мини = находки
                    .iter()
                    .map(|н| match online::миниатюра(н) {
                        Some(байты) => разобрать_миниатюру(&байты),
                        None => пустая_миниатюра(),
                    })
                    .collect();
                ИзСети::Список(находки, мини)
            }
        };
        let _ = tx.send(ответ);
    });
}

/// Скачать выбранные в сети обои и сразу их поставить.
fn взять_из_сети(state: &mut Config, idx: usize) {
    if state.сетевая_работа { return; }
    let Some(находка) = state.находки.get(idx).cloned() else { return };

    let (tx, rx) = std::sync::mpsc::channel();
    state.pending_online = Some(rx);
    state.сетевая_работа = true;
    state.сетевая_ошибка = false;
    state.dirty = true;

    std::thread::spawn(move || {
        let ответ = match online::скачать(&находка, &каталог_загрузок()) {
            Ok(путь) => ИзСети::Скачано(путь),
            Err(e) => ИзСети::Ошибка(e),
        };
        let _ = tx.send(ответ);
    });
}

fn пустая_миниатюра() -> (u32, u32, Vec<u8>) {
    (THUMB, THUMB, vec![60u8; (THUMB * THUMB * 4) as usize])
}

/// Разобрать скачанную миниатюру в карточку сетки.
fn разобрать_миниатюру(байты: &[u8]) -> (u32, u32, Vec<u8>) {
    match image::load_from_memory(байты) {
        Ok(img) => {
            let (w, h) = img.dimensions();
            (THUMB, THUMB, resize(w, h, &img.to_rgba8().into_raw(), THUMB, THUMB))
        }
        // Битая или неизвестного формата — показываем серую заглушку, но
        // карточку оставляем: сами обои могут быть в порядке.
        Err(_) => пустая_миниатюра(),
    }
}

/// Пролистать сетку на `шаг` рядов, не вылезая за края списка.
fn прокрутить(state: &mut Config, шаг: i32) {
    let макс = предел_прокрутки(state) as i32;
    let новый = (state.scroll_row as i32 + шаг).clamp(0, макс) as u32;

    // Докрутили до низа сетевой выдачи — подтягиваем следующую страницу.
    // Так «листать обои» работает бесконечно, без отдельной кнопки «ещё».
    if шаг > 0 && новый == макс as u32 && state.вкладка != Вкладка::Мои
        && !state.сетевая_работа && !state.находки.is_empty()
    {
        загрузить_страницу(state, state.сетевая_страница + 1, true);
    }

    if новый == state.scroll_row { return; }
    state.scroll_row = новый;
    // Хит-боксы привязаны к рядам — после сдвига их надо пересобрать, иначе
    // клик уходил бы по карточке, которая уже уехала.
    build_ui_layout(state);
    state.hover = hover_cell(state);
    state.dirty = true;
}

fn is_hover(state: &Config, x: u32, y: u32, w: u32, h: u32) -> bool {
    if state.cursor_x < 0.0 || state.cursor_y < 0.0 { return false; }
    let cx = state.cursor_x as u32;
    let cy = state.cursor_y as u32;
    cx >= x && cx < x + w && cy >= y && cy < y + h
}

/// Единственный источник правды о сетке карточек: и рисование, и хит-боксы
/// ходят сюда. Раньше эта арифметика была продублирована в render_overlay и
/// build_ui_layout — правка в одном месте разъезжалась с другим, и картинка
/// нажималась мимо.
///
/// Возвращает (индекс, x, y); индекс == wallpapers.len() — это карточка «+».
///
/// `scroll` — номер первого видимого ряда. Прокрутка идёт ЦЕЛЫМИ рядами, а не
/// пикселями: карточка тогда либо видна целиком, либо не рисуется вовсе, и не
/// нужно ни обрезать миниатюры по краю окна, ни держать хит-боксы с
/// отрицательной координатой (а они у нас в u32).
fn ui_cells(n: usize, scroll: u32) -> Vec<(usize, u32, u32)> {
    // n+1 карточек: сами обои плюс «+» в конце.
    сетка(n + 1, scroll)
}

/// Та же сетка, но по ТОЧНОМУ числу карточек. У сетевой выдачи карточки «+»
/// нет — добавлять в неё нечего.
fn сетка(всего: usize, scroll: u32) -> Vec<(usize, u32, u32)> {
    let mx = (UI_W - COLS * THUMB - (COLS - 1) * GAP) / 2;
    let mut cells = Vec::new();
    for idx in 0..всего {
        let row = idx as u32 / COLS;
        if row < scroll { continue; }
        let экранный = row - scroll;
        if экранный >= VIS_ROWS { break; }
        let col = idx as u32 % COLS;
        let x = mx + col * (THUMB + GAP);
        let y = MY + экранный * (THUMB + GAP);
        cells.push((idx, x, y));
    }
    cells
}

/// Всего рядов в сетке, считая ряд с карточкой «+».
fn всего_рядов(n: usize) -> u32 {
    рядов(n + 1)
}

fn рядов(всего: usize) -> u32 {
    (всего as u32 + COLS - 1) / COLS
}

/// На сколько рядов список можно прокрутить вниз.
fn макс_прокрутка(n: usize) -> u32 {
    всего_рядов(n).saturating_sub(VIS_ROWS)
}

/// Прямоугольники кнопок-вкладок: свои обои, картинки, живые.
///
/// Подписей нет — в dwall нет шрифта, поэтому вкладки различаются значками
/// (сетка, гора-фотография, треугольник «плей»), а выбранная подсвечена.
fn tab_rects() -> [(u32, u32); 3] {
    let mx = (UI_W - COLS * THUMB - (COLS - 1) * GAP) / 2;
    [(mx, 16), (mx + ARROW + 8, 16), (mx + 2 * (ARROW + 8), 16)]
}

/// Кнопка поиска — сразу за вкладками.
fn search_rect() -> (u32, u32) {
    let mx = (UI_W - COLS * THUMB - (COLS - 1) * GAP) / 2;
    (mx + 3 * (ARROW + 8) + 6, 16)
}

const ВКЛАДКИ: [Вкладка; 3] = [Вкладка::Мои, Вкладка::Картинки, Вкладка::Живые];

/// Прямоугольники стрелок прокрутки: (вверх, вниз).
fn arrow_rects() -> [(u32, u32); 2] {
    let x = UI_W - ARROW - 12;
    [(x, 16), (x, UI_H - ARROW - 16)]
}

/// Треугольник стрелки прокрутки.
fn draw_arrow(pixels: &mut [u8], sw: u32, sh: u32, x: u32, y: u32, size: u32, вниз: bool, цвет: u32) {
    let половина = size / 2;
    for row in 0..половина {
        // Чем ближе к острию, тем короче строка.
        let ширина = (половина - row) * 2;
        if ширина == 0 { continue; }
        let ry = if вниз { y + size / 4 + row } else { y + size - size / 4 - row };
        let rx = x + size / 2 - ширина / 2;
        for col in 0..ширина {
            let (px, py) = (rx + col, ry);
            if px >= sw || py >= sh { continue; }
            let o = ((py * sw + px) * 4) as usize;
            if o + 3 < pixels.len() {
                pixels[o] = ((цвет >> 16) & 0xFF) as u8;
                pixels[o + 1] = ((цвет >> 8) & 0xFF) as u8;
                pixels[o + 2] = (цвет & 0xFF) as u8;
                pixels[o + 3] = ((цвет >> 24) & 0xFF) as u8;
            }
        }
    }
}

/// Крестик кнопки удаления: две диагонали внутри круга.
fn draw_cross(pixels: &mut [u8], sw: u32, sh: u32, x: u32, y: u32, size: u32) {
    let pad = size / 3;
    let n = size.saturating_sub(2 * pad);
    for i in 0..n {
        for t in 0..2u32 {
            let px1 = x + pad + i;
            let py1 = y + pad + i + t;
            let px2 = x + pad + i;
            let py2 = y + size - pad - i - 1 + t;
            for (px, py) in [(px1, py1), (px2, py2)] {
                if px < sw && py < sh {
                    let o = ((py * sw + px) * 4) as usize;
                    if o + 3 < pixels.len() {
                        pixels[o] = ((DEL_FG >> 16) & 0xFF) as u8;
                        pixels[o + 1] = ((DEL_FG >> 8) & 0xFF) as u8;
                        pixels[o + 2] = (DEL_FG & 0xFF) as u8;
                        pixels[o + 3] = ((DEL_FG >> 24) & 0xFF) as u8;
                    }
                }
            }
        }
    }
}

/// Значок поиска — лупа: кольцо и ручка.
fn draw_search_icon(pixels: &mut [u8], x: u32, y: u32, цвет: u32) {
    let cx = x + ARROW / 2 - 2;
    let cy = y + ARROW / 2 - 2;
    let r = 6i32;
    // Кольцо рисуем по окружности: точек мало, тригонометрия тут дешевле
    // любой заливки.
    for угол in 0..90 {
        let a = угол as f64 * std::f64::consts::PI / 45.0;
        let px = cx as i32 + (r as f64 * a.cos()).round() as i32;
        let py = cy as i32 + (r as f64 * a.sin()).round() as i32;
        if px >= 0 && py >= 0 {
            fill_rounded_rect(pixels, UI_W, UI_H, px as u32, py as u32, 2, 2, 0, цвет);
        }
    }
    // Ручка — по диагонали вниз-вправо от кольца.
    for i in 0..6u32 {
        fill_rounded_rect(pixels, UI_W, UI_H, cx + 5 + i, cy + 5 + i, 2, 2, 0, цвет);
    }
}

/// Значок вкладки. Текста в меню нет, поэтому вкладки различаются рисунком:
/// решётка — свои обои, гора в рамке — картинки из сети, треугольник — живые.
fn draw_tab_icon(pixels: &mut [u8], вкладка: Вкладка, x: u32, y: u32, цвет: u32) {
    let s = ARROW;
    match вкладка {
        Вкладка::Мои => {
            // Четыре квадратика 2×2 — «моя коллекция».
            let d = s / 2 - 3;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                fill_rounded_rect(
                    pixels, UI_W, UI_H,
                    x + 6 + dx * (d + 2), y + 6 + dy * (d + 2),
                    d, d, 1, цвет,
                );
            }
        }
        Вкладка::Картинки => {
            // Рамка с «горой» внутри — привычный значок изображения.
            fill_rounded_rect(pixels, UI_W, UI_H, x + 5, y + 7, s - 10, 2, 1, цвет);
            fill_rounded_rect(pixels, UI_W, UI_H, x + 5, y + s - 9, s - 10, 2, 1, цвет);
            fill_rounded_rect(pixels, UI_W, UI_H, x + 5, y + 7, 2, s - 14, 1, цвет);
            fill_rounded_rect(pixels, UI_W, UI_H, x + s - 7, y + 7, 2, s - 14, 1, цвет);
            let низ = y + s - 10;
            for i in 0..(s / 2 - 4) {
                let ширина = i * 2;
                if ширина == 0 { continue; }
                fill_rounded_rect(pixels, UI_W, UI_H, x + s / 2 - i, низ - i, ширина, 1, 0, цвет);
            }
        }
        Вкладка::Живые => {
            // Треугольник «плей» — движущиеся обои.
            let высота = s - 12;
            for ряд in 0..высота {
                // Ширина сходится к острию: до середины растёт, дальше падает.
                let от_края = if ряд < высота / 2 { ряд } else { высота - 1 - ряд };
                let ширина = (от_края * (s - 14) * 2 / высота.max(1)).max(1);
                fill_rounded_rect(
                    pixels, UI_W, UI_H,
                    x + 8, y + 6 + ряд, ширина, 1, 0, цвет,
                );
            }
        }
    }
}

/// Сколько карточек в сетке сейчас: свои обои плюс «+», либо находки из сети.
fn карточек(state: &Config) -> usize {
    match state.вкладка {
        Вкладка::Мои => state.wallpapers.len() + 1,
        _ => state.находки.len(),
    }
}

fn предел_прокрутки(state: &Config) -> u32 {
    рядов(карточек(state)).saturating_sub(VIS_ROWS)
}

fn build_ui_layout(state: &mut Config) {
    state.hit_rects.clear();
    let n = state.wallpapers.len();
    // Список мог укоротиться (удалили обои) — не оставляем прокрутку за краем,
    // иначе меню открывалось бы на пустом месте.
    state.scroll_row = state.scroll_row.min(предел_прокрутки(state));

    // Вкладки — поверх всего: они в собственной полосе над сеткой.
    for (i, (tx, ty)) in tab_rects().iter().enumerate() {
        state.hit_rects.push(HitRect {
            x: *tx, y: *ty, w: ARROW, h: ARROW,
            action: UiAction::Вкладка(ВКЛАДКИ[i]),
        });
    }
    let (sx, sy) = search_rect();
    state.hit_rects.push(HitRect { x: sx, y: sy, w: ARROW, h: ARROW, action: UiAction::Поиск });

    // Сетевая выдача: своя раскладка, без карточки «+» и без крестиков —
    // удалять из чужого каталога нечего.
    if state.вкладка != Вкладка::Мои {
        for (idx, x, y) in сетка(state.находки.len(), state.scroll_row) {
            state.hit_rects.push(HitRect {
                x, y, w: THUMB, h: THUMB,
                action: UiAction::ВзятьИзСети(idx),
            });
        }
        if предел_прокрутки(state) > 0 {
            let [(ax, ay_up), (_, ay_dn)] = arrow_rects();
            state.hit_rects.push(HitRect { x: ax, y: ay_up, w: ARROW, h: ARROW, action: UiAction::Scroll(-1) });
            state.hit_rects.push(HitRect { x: ax, y: ay_dn, w: ARROW, h: ARROW, action: UiAction::Scroll(1) });
        }
        return;
    }

    // Стрелки кладём первыми — они перекрывают поле, а не карточки.
    if макс_прокрутка(n) > 0 {
        let [(ax, ay_up), (_, ay_dn)] = arrow_rects();
        state.hit_rects.push(HitRect {
            x: ax, y: ay_up, w: ARROW, h: ARROW, action: UiAction::Scroll(-1),
        });
        state.hit_rects.push(HitRect {
            x: ax, y: ay_dn, w: ARROW, h: ARROW, action: UiAction::Scroll(1),
        });
    }

    for (idx, x, y) in ui_cells(n, state.scroll_row) {
        // Крестик удаления кладём ПЕРВЫМ: попадание ищется по порядку, и он
        // должен перехватывать клик раньше, чем сама карточка (иначе удаление
        // было бы недоступно — карточка накрывает его целиком).
        if idx < n {
            state.hit_rects.push(HitRect {
                x: x + THUMB - DEL_SIZE - 4,
                y: y + 4,
                w: DEL_SIZE,
                h: DEL_SIZE,
                action: UiAction::DeleteWallpaper(idx),
            });
        }
        let action = if idx < n { UiAction::SelectWallpaper(idx) } else { UiAction::AddWallpaper };
        state.hit_rects.push(HitRect { x, y, w: THUMB, h: THUMB, action });
    }
}

/// Считает миниатюры. Дорого: каждая картинка декодируется с диска целиком
/// (обои — это обычно 4K PNG), поэтому результат кэшируется по списку обоев.
/// Раньше вызов шёл на КАЖДОЕ открытие меню, и оно заметно подвисало перед
/// появлением; теперь пересчёт только когда список реально изменился —
/// например после добавления обоев плюсиком.
/// Запускает декодирование обоев в фоне. Результат подхватит главный цикл.
fn start_load_wallpaper(state: &mut Config, path: String) {
    save_current(&path);

    // Палитра считается в СВОЁМ потоке и ничего не ждёт: она никак не влияет
    // на показ обоев, а декодирование картинки под гистограмму стоит столько
    // же, сколько сами обои, — в главном цикле это была бы заметная пауза.
    {
        let p = path.clone();
        std::thread::spawn(move || palette::apply(&p));
    }

    // Снимок того, что сейчас на экране, — с него начнётся плавный переход.
    // Берём именно показанный буфер, а не исходную картинку: так одинаково
    // работает и для статичных обоев, и для видео, где «текущего кадра» в
    // виде файла попросту нет.
    начать_переход(state);
    // Прошлые живые обои останавливаем сразу: Drop убьёт ffmpeg, а поток
    // gif-декодера сам выйдет, когда канал окажется закрыт.
    state.anim = None;

    // Живые форматы (видео и многокадровый gif) идут своим путём — кадрами.
    let (w, h) = state.outputs.first().map(|(_, w, h)| (*w, *h)).unwrap_or((0, 0));
    if w > 0 && h > 0 && (is_video(&path) || path.to_lowercase().ends_with(".gif")) {
        if let Some(anim) = start_anim(&path, w, h) {
            state.anim = Some(anim);
            state.pending_img = None;
            return;
        }
        // Не вышло (одиночный gif, нет ffmpeg) — падаем в обычную картинку.
    }

    let (tx, rx) = std::sync::mpsc::channel();
    state.pending_img = Some(rx);
    std::thread::spawn(move || {
        let _ = tx.send(load_image(&path));
    });
}

fn load_thumbnails(state: &mut Config) {
    if state.thumbs_for == state.wallpapers && !state.thumbnails.is_empty() {
        return;
    }
    if state.pending_thumbs.is_some() {
        return; // уже считаются
    }
    state.thumbs_for = state.wallpapers.clone();
    let пути = state.wallpapers.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    state.pending_thumbs = Some(rx);
    std::thread::spawn(move || {
        let mut out = Vec::with_capacity(пути.len());
        for path in &пути {
            if let Some((w, h, px)) = load_image(path) {
                out.push((THUMB, THUMB, resize(w, h, &px, THUMB, THUMB)));
            } else {
                out.push((THUMB, THUMB, vec![60u8; (THUMB * THUMB * 4) as usize]));
            }
        }
        let _ = tx.send(out);
    });
}

fn draw_thumbnail(pixels: &mut [u8], sw: u32, _sh: u32,
                  dx: u32, dy: u32, dw: u32, dh: u32,
                  thumb: &(u32, u32, Vec<u8>)) {
    let (tw, th, data) = thumb;
    let ox = if *tw > dw { (*tw - dw) / 2 } else { 0 };
    let oy = if *th > dh { (*th - dh) / 2 } else { 0 };
    for row in 0..dh {
        for col in 0..dw {
            let sx = ox + col; let sy = oy + row;
            if sx >= *tw || sy >= *th { continue; }
            let si = ((sy * tw + sx) * 4) as usize;
            let di = (((dy + row) * sw + (dx + col)) * 4) as usize;
            if si + 3 < data.len() && di + 3 < pixels.len() {
                pixels[di..di+4].copy_from_slice(&data[si..si+4]);
            }
        }
    }
}

fn draw_plus(pixels: &mut [u8], sw: u32, _sh: u32, dx: u32, dy: u32, size: u32) {
    let cx = dx + size / 2;
    let cy = dy + size / 2;
    let t = 4; // thickness
    let l = 40; // line length
    for row in (cy - t/2)..(cy + t/2 + 1) {
        for col in (cx - l/2)..(cx + l/2 + 1) {
            let idx = ((row * sw + col) * 4) as usize;
            if idx + 3 < pixels.len() { pixels[idx..idx+4].copy_from_slice(&[200, 200, 200, 255]); }
        }
    }
    for row in (cy - l/2)..(cy + l/2 + 1) {
        for col in (cx - t/2)..(cx + t/2 + 1) {
            let idx = ((row * sw + col) * 4) as usize;
            if idx + 3 < pixels.len() { pixels[idx..idx+4].copy_from_slice(&[200, 200, 200, 255]); }
        }
    }
}

// ── Rounded rect ─────────────────────────────────────────────────────────────

fn fill_rounded_rect(pixels: &mut [u8], sw: u32, _sh: u32,
                     x: u32, y: u32, w: u32, h: u32, r: u32, color: u32) {
    let a = (color >> 24) & 0xFF;
    let rc = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    let rgba = [rc as u8, g as u8, b as u8, a as u8];

    let corner = |r: u32, row: u32| -> u32 {
        let d = r.saturating_sub(row);
        let xc = ((r as f64 * r as f64 - d as f64 * d as f64).sqrt() + 0.5) as u32;
        r.saturating_sub(xc)
    };

    for row in 0..h {
        let cy = y + row;
        if cy >= _sh { break; }
        let left = if row < r { corner(r, row) } else { 0 };
        let right = if row >= h.saturating_sub(r) { corner(r, h - 1 - row) } else { 0 };
        let cs = x + left;
        let ce = x + w - right;
        if ce <= cs { continue; }
        for col in cs..ce {
            let idx = ((cy * sw + col) * 4) as usize;
            if idx + 3 < pixels.len() { pixels[idx..idx+4].copy_from_slice(&rgba); }
        }
    }
}

// ── SHM helpers ──────────────────────────────────────────────────────────────

fn create_shm_fd(size: usize) -> Option<(i32, *mut libc::c_void)> {
    let fd = unsafe { libc::memfd_create(c"dwall".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { return None; }
    // ftruncate может не дотянуть до size (например, tmpfs кончился) — тогда
    // mmap отдаст область короче буфера, и запись уедет за её край.
    if unsafe { libc::ftruncate(fd, size as i64) } < 0 {
        unsafe { libc::close(fd); }
        return None;
    }
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if ptr == libc::MAP_FAILED { unsafe { libc::close(fd); } return None; }
    Some((fd, ptr))
}

// ── Image loading ────────────────────────────────────────────────────────────

fn load_image(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    // Видео: берём один кадр из середины — он идёт и в миниатюру, и как
    // статичная подложка, пока не раскрутится живой декодер.
    if is_video(path) {
        return first_video_frame(path);
    }
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "gif" => {
            // Первый кадр — тоже заплатка со своим прямоугольником, поэтому
            // кладём его на холст логического размера (см. start_anim). Без
            // этого миниатюра и статичная подложка показывали кусок кадра,
            // растянутый на всю карточку.
            let file = std::fs::File::open(path).ok()?;
            let mut d = gif::DecodeOptions::new();
            d.set_color_output(gif::ColorOutput::RGBA);
            let mut r = d.read_info(file).ok()?;
            let (sw, sh) = (r.width() as usize, r.height() as usize);
            let f = r.read_next_frame().ok()??;
            if sw == 0 || sh == 0 { return None; }
            let mut холст = vec![0u8; sw * sh * 4];
            let (fx, fy) = (f.left as usize, f.top as usize);
            let (fw, fh) = (f.width as usize, f.height as usize);
            for y in 0..fh {
                let dy = fy + y;
                if dy >= sh { break; }
                for x in 0..fw {
                    let dx = fx + x;
                    if dx >= sw { break; }
                    let s = (y * fw + x) * 4;
                    if s + 3 >= f.buffer.len() || f.buffer[s + 3] == 0 { continue; }
                    let d = (dy * sw + dx) * 4;
                    холст[d..d + 4].copy_from_slice(&f.buffer[s..s + 4]);
                }
            }
            for c in холст.chunks_exact_mut(4) { c[3] = 255; }
            Some((sw as u32, sh as u32, холст))
        }
        _ => {
            image::open(path).ok().map(|img| { let (w, h) = img.dimensions(); (w, h, img.to_rgba8().into_raw()) })
        }
    }
}

/// Возвращает картинку, растянутую под выход. Пустой/битый вход не роняет
/// клиент: from_raw отдаёт None, если длина буфера не сходится с iw*ih*4.
/// Масштабирование «ближайшим соседом» напрямую в целевой буфер.
///
/// Исходник растягивается на весь `dst` (как resize_to_fill: по большей
/// стороне, лишнее обрезается). Никаких промежуточных буферов: строка
/// источника выбирается по таблице, столбцы — по заранее посчитанным
/// индексам, дальше только запись 4 байт на пиксель.
fn blit_scaled(src: &[u8], iw: u32, ih: u32, dst: &mut [u8], ow: u32, oh: u32, bgra: bool) {
    if iw == 0 || ih == 0 || ow == 0 || oh == 0 { return; }
    // «Заполнить»: берём общий масштаб по большей стороне и центрируем.
    let k = (ow as f64 / iw as f64).max(oh as f64 / ih as f64);
    let вид_w = (ow as f64 / k).min(iw as f64).max(1.0);
    let вид_h = (oh as f64 / k).min(ih as f64).max(1.0);
    let off_x = (iw as f64 - вид_w) / 2.0;
    let off_y = (ih as f64 - вид_h) / 2.0;

    // Таблица столбцов считается один раз на кадр, а не для каждой строки.
    let mut cols = vec![0usize; ow as usize];
    for (x, c) in cols.iter_mut().enumerate() {
        let sx = (off_x + (x as f64 + 0.5) * вид_w / ow as f64) as usize;
        *c = sx.min(iw as usize - 1) * 4;
    }

    for y in 0..oh as usize {
        let sy = ((off_y + (y as f64 + 0.5) * вид_h / oh as f64) as usize).min(ih as usize - 1);
        let строка = sy * iw as usize * 4;
        let выход = y * ow as usize * 4;
        for x in 0..ow as usize {
            let i = строка + cols[x];
            let o = выход + x * 4;
            if i + 3 >= src.len() || o + 3 >= dst.len() { continue; }
            if bgra {
                dst[o] = src[i];
                dst[o + 1] = src[i + 1];
                dst[o + 2] = src[i + 2];
            } else {
                dst[o] = src[i + 2];
                dst[o + 1] = src[i + 1];
                dst[o + 2] = src[i];
            }
            dst[o + 3] = src[i + 3];
        }
    }
}

fn resize_ex(iw: u32, ih: u32, px: &[u8], ow: u32, oh: u32, быстро: bool) -> Vec<u8> {
    let Some(img) = image::RgbaImage::from_raw(iw, ih, px.to_vec()) else {
        return vec![35u8; (ow * oh * 4) as usize];
    };
    // Для анимации — Nearest: gif обычно мелкий (320×180), а тянуть его надо
    // на весь экран по десять раз в секунду. Triangle на этом съедал пятую
    // часть ядра, Nearest почти бесплатен, а на растянутых в семь раз обоях
    // разницу увидеть трудно.
    let f = if быстро { FilterType::Nearest } else { FilterType::Lanczos3 };
    DynamicImage::ImageRgba8(img).resize_to_fill(ow, oh, f).to_rgba8().into_raw()
}

fn resize(iw: u32, ih: u32, px: &[u8], ow: u32, oh: u32) -> Vec<u8> {
    let Some(img) = image::RgbaImage::from_raw(iw, ih, px.to_vec()) else {
        return vec![35u8; (ow * oh * 4) as usize];
    };
    DynamicImage::ImageRgba8(img)
        .resize_to_fill(ow, oh, FilterType::Lanczos3).to_rgba8().into_raw()
}

/// Диалог выбора файла для карточки «+».
///
/// zenity в системе нет (это NixOS без GTK-обвязки), поэтому спрашиваем
/// каждого кандидата по очереди и берём первого установленного. fuzzel —
/// нативный Wayland-launcher, он тут есть; список файлов ему подаём сами,
/// собрав картинки из стандартных каталогов.
fn pick_wallpaper_file() -> Option<String> {
    // Графические диалоги, если вдруг появятся в системе.
    for (bin, args) in [
        ("zenity", &["--file-selection", "--title=Выберите обои"][..]),
        ("kdialog", &["--getopenfilename"][..]),
        ("yad", &["--file", "--title=Выберите обои"][..]),
    ] {
        if which(bin).is_none() { continue; }
        let out = std::process::Command::new(bin).args(args).output().ok()?;
        if !out.status.success() { return None; }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return if path.is_empty() { None } else { Some(path) };
    }

    // Фолбэк: свой список картинок через dmenu-подобный выбор.
    let candidates = scan_image_files();
    if candidates.is_empty() { return None; }
    for (bin, args) in [
        ("fuzzel", &["--dmenu", "--prompt=обои: "][..]),
        ("wofi", &["--dmenu"][..]),
        ("rofi", &["-dmenu", "-p", "обои"][..]),
        ("bemenu", &["-p", "обои"][..]),
        ("dmenu", &["-p", "обои"][..]),
    ] {
        if which(bin).is_none() { continue; }
        return run_dmenu(bin, args, &candidates);
    }
    None
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(|d| format!("{d}/{bin}"))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))
}

/// Скормить список строк dmenu-подобной утилите и вернуть выбранную.
fn run_dmenu(bin: &str, args: &[&str], items: &[String]) -> Option<String> {
    use std::io::Write;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn().ok()?;
    // stdin закрываем до wait_with_output, иначе dmenu ждёт EOF, а мы — его.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(items.join("\n").as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() { return None; }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Картинки из привычных каталогов — то, из чего пользователю предлагаем выбрать.
fn scan_image_files() -> Vec<String> {
    let home = home();
    let mut out = Vec::new();
    for dir in [
        format!("{home}/Pictures"),
        format!("{home}/Pictures/Wallpapers"),
        format!("{home}/Загрузки"),
        format!("{home}/Downloads"),
        format!("{home}/.config/dwall"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if is_image(&p) { out.push(p.to_string_lossy().to_string()); }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Один кадр видео размером 640×360 — для миниатюры и статичной подложки.
fn first_video_frame(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    const W: u32 = 640;
    const H: u32 = 360;
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel", "error", "-nostdin",
            "-ss", "1", "-i", path, "-frames:v", "1",
            "-vf", &format!("scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H}"),
            "-f", "rawvideo", "-pix_fmt", "rgba", "-",
        ])
        .output()
        .ok()?;
    let нужно = (W * H * 4) as usize;
    if out.stdout.len() < нужно {
        return None;
    }
    Some((W, H, out.stdout[..нужно].to_vec()))
}

fn is_image(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str(),
        // Картинки — через image/gif, видео — через ffmpeg (см. start_anim).
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            | "mp4" | "webm" | "mkv" | "mov" | "avi"
    )
}

// ── Config persistence ───────────────────────────────────────────────────────

fn home() -> String { std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()) }

fn load_config(state: &mut Config) {
    let path = format!("{}/.config/dwall/wallpapers.json", home());
    if let Ok(data) = std::fs::read_to_string(&path) {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&data) { state.wallpapers = list; return; }
    }
    let dir = format!("{}/.config/dwall", home());
    let mut list = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if is_image(&p) { list.push(p.to_string_lossy().to_string()); }
        }
    }
    list.sort();
    state.wallpapers = list;
}

/// Путь к файлу с ВЫБРАННЫМИ обоями. Список хранится отдельно (wallpapers.json)
/// и порядок в нём может меняться, поэтому запоминаем сам путь, а не индекс.
fn current_path_file() -> String {
    format!("{}/.config/dwall/current", home())
}

/// Запомнить, какие обои показаны сейчас.
fn save_current(path: &str) {
    let p = current_path_file();
    if let Some(d) = std::path::Path::new(&p).parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(&p, path);
}

/// Прочитать запомненные обои (если файл ещё существует).
fn load_current() -> Option<String> {
    let s = std::fs::read_to_string(current_path_file()).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty() && std::path::Path::new(&s).exists()).then_some(s)
}

fn save_config(wallpapers: &[String]) {
    let path = format!("{}/.config/dwall/wallpapers.json", home());
    if let Some(d) = std::path::Path::new(&path).parent() { let _ = std::fs::create_dir_all(d); }
    if let Ok(data) = serde_json::to_string_pretty(wallpapers) { let _ = std::fs::write(&path, &data); }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Сетка без прокрутки показывает первые VIS_ROWS рядов, и ни карточкой
    /// больше: до этой правки ui_cells обрывал список по краю окна, и всё, что
    /// не влезло, было недоступно вообще.
    #[test]
    fn сетка_ограничена_видимыми_рядами() {
        let cells = ui_cells(40, 0);
        assert_eq!(cells.len() as u32, COLS * VIS_ROWS);
        assert_eq!(cells[0], (0, cells[0].1, MY));
        // Последний видимый ряд стоит ровно на своём месте.
        let последний = cells.last().unwrap();
        assert_eq!(последний.2, MY + (VIS_ROWS - 1) * (THUMB + GAP));
        // И ничего не вылезает за нижний край окна.
        assert!(последний.2 + THUMB <= UI_H);
    }

    /// Прокрутка сдвигает окно ровно на ряд и не показывает уехавшие карточки.
    #[test]
    fn прокрутка_сдвигает_на_ряд() {
        let первый = ui_cells(40, 0);
        let второй = ui_cells(40, 1);
        assert_eq!(второй[0].0 as u32, COLS, "первым идёт начало второго ряда");
        assert_eq!(второй[0].2, MY, "и он поднят к верхнему краю сетки");
        assert_eq!(первый[0].1, второй[0].1, "столбцы на месте");
    }

    /// Прокручивать можно ровно настолько, чтобы последний ряд встал внизу.
    #[test]
    fn предел_прокрутки() {
        // 3 обоев + «+» = 4 карточки = один ряд: листать нечего.
        assert_eq!(макс_прокрутка(3), 0);
        // Ровно VIS_ROWS рядов — тоже нечего.
        assert_eq!(макс_прокрутка((COLS * VIS_ROWS - 1) as usize), 0);
        // Ещё одна карточка — появляется один шаг.
        assert_eq!(макс_прокрутка((COLS * VIS_ROWS) as usize), 1);
        // На пределе последний ряд непустой.
        let n = 40usize;
        let cells = ui_cells(n, макс_прокрутка(n));
        assert!(cells.iter().any(|(idx, _, _)| *idx == n), "карточка «+» должна быть видна в самом низу");
    }

    /// Дальние от обоев тона не должны терять опознаваемость: синий обязан
    /// остаться синим даже на подчёркнуто тёплых обоях.
    #[test]
    fn тон_не_уезжает_дальше_потолка() {
        let палитра = palette::Palette::from_pixels(&[200, 120, 60, 255].repeat(64 * 64));
        let синий = палитра.ansi[4];
        assert!(синий[2] > синий[0], "синий остался синим: {синий:?}");
        let красный = палитра.ansi[1];
        assert!(красный[0] > красный[2], "красный остался красным: {красный:?}");
    }
}
