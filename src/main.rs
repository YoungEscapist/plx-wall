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

mod ffmpeg_sys;
mod hwvideo;
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
        wl_callback::WlCallback,
    },
};
// wp_viewporter: «буфер такой-то, а показывай его вот в этом прямоугольнике».
// Растягивание при этом делает композитор на видеокарте, а не мы на процессоре.
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
// linux-dmabuf: буфер, который лежит в видеопамяти и никогда не приезжал в
// системную. Через него показываются живые обои, декодированные видеокартой
// (см. hwvideo.rs) — процессор не касается ни одного пикселя.
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
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
    /// Статичные картинки с openverse.
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
    /// Показать меню фильтров выдачи.
    Фильтр,
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
    /// Размер снимка. С viewporter буфер равен размеру КАДРА, а не экрана, и у
    /// новых обоев он свой — снимок приходится подгонять (см. render_wallpaper).
    dims: (u32, u32),
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
    /// Тот самый memfd, поверх которого лежит `ptr`.
    ///
    /// Раньше он закрывался сразу после создания буфера — отображение держит
    /// себя само. Теперь он нужен живым: декодер заливает кадр прямо в общую
    /// память через `splice`, а `splice` умеет писать только в файловый
    /// дескриптор (см. [`Заём`]).
    fd: OwnedFd,
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
        // owned дальше переезжает в сам слот — см. поле ShmSlot::fd.
        let free = std::sync::Arc::new(AtomicBool::new(true));
        let buffer = pool.create_buffer(
            0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, free.clone(),
        );
        pool.destroy(); // отображение держит сам буфер
        out.push(ShmSlot { buffer, ptr: ptr as *mut u8, len: size, free, fd: owned });
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

/// Что декодер прислал главному циклу.
enum Кадр {
    /// Видео: кадр УЖЕ лежит в общей памяти, в слоте с этим номером.
    ///
    /// Так дешевле всего: `splice` перекладывает страницы трубы прямо в memfd
    /// слота, и ни одного копирования на нашей стороне не остаётся. Замер
    /// 14.08.2026 на 2560×1080 (свой стенд, 120 кадров): «прочитать в буфер и
    /// скопировать в общую память» — 12.77 мс CPU на кадр, `splice` — 7.94 мс.
    ВСлоте { слот: usize, шаг: std::time::Duration },
    /// gif: кадр приходит своим буфером. Там он собирается на холсте из
    /// заплаток (см. декодер gif), общей памятью это не заменить, да и незачем
    /// — гифы мелкие.
    Буфером { w: u32, h: u32, px: Vec<u8>, шаг: std::time::Duration },
}

/// Слот, отданный декодеру во временное пользование.
///
/// Внутри — ДУБЛИКАТ дескриптора, а не сам слот: обои могут смениться, старые
/// слоты уйдут вместе с их памятью, и опоздавший декодер зальёт кадр в свою
/// копию memfd, которую никто не читает. Без дубликата он писал бы по номеру
/// дескриптора, который ядро успело выдать чему-то другому.
struct Заём {
    индекс: usize,
    fd: OwnedFd,
}

/// Проигрыватель живых обоев.
///
/// Кадры готовит фоновый поток и кладёт в канал ёмкостью 1 — он же служит
/// тормозом: пока главный цикл не забрал кадр, декодер стоит, и память не
/// растёт (у видео кадр в размер экрана — это 11 МБ, держать очередь нельзя).
/// Темп задаёт главный цикл по `delay`, а не декодер.
struct Anim {
    /// Кадр: ширина, высота, пиксели, СОБСТВЕННАЯ длительность кадра и признак
    /// «уже BGRA». Длительность — Duration, а не миллисекунды: у видео 60 к/с
    /// это 16 666 мкс, и округление до 16 мс дало бы 62.5 к/с вместо 60.
    rx: std::sync::mpsc::Receiver<Кадр>,
    /// Куда главный цикл кладёт свободные слоты для декодера. Он же и тормоз:
    /// пока слотов нет (кадр показывается, обои закрыты полноэкранным окном),
    /// декодер стоит на `recv`, очередь ffmpeg переполняется, и тот засыпает
    /// сам.
    слоты: std::sync::mpsc::Sender<Заём>,
    /// Размер кадра, который отдаёт декодер. Нужен ЗАРАНЕЕ: слоты общей памяти
    /// заводятся под него ещё до первого кадра — заливать-то декодер будет
    /// прямо в них.
    dims: (u32, u32),
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

/// Кодек видеодорожки (`h264`, `vp9`, `av1`…) — по нему подбирается декодер
/// NVDEC, см. [`cuvid_аргументы`].
fn кодек_видео(path: &str) -> Option<String> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=codec_name",
            "-of", "default=nw=1:nk=1",
            path,
        ])
        .output()
        .ok()?;
    let имя = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!имя.is_empty()).then_some(имя)
}

/// Есть ли в этой сборке ffmpeg декодер `<кодек>_cuvid`.
fn cuvid_есть(кодек: &str) -> bool {
    static СПИСОК: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let список = СПИСОК.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-decoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    });
    список.contains(&format!("{кодек}_cuvid"))
}

/// Аргументы, которыми ресайз обоев переносится ВНУТРЬ видеокарты: декодер
/// `<кодек>_cuvid` умеет сам обрезать и ужимать кадр (`-crop`, `-resize`), и
/// тогда на процессоре не остаётся ни масштабирования, ни лишнего кадра 4K.
///
/// Замер 12.08.2026 на обоях 3840×2160@60 h264 → экран 2560×1080, 120 кадров:
///
/// | как декодируем | CPU |
/// |---|---|
/// | программно | 8.13 с |
/// | `-hwaccel cuda`, `scale` в swscale (было) | 3.67 с |
/// | `h264_cuvid` с `-crop`/`-resize` (стало) | **2.29 с** |
///
/// `sws_flags` при этом не значит ничего (проверено на fast_bilinear, neighbor,
/// bilinear — разброс в пределах погрешности): платим не за качество фильтра, а
/// за сам пересчёт 4K и за перевод nv12→bgra.
///
/// Возвращает (декодер, `-crop`, `-resize`). Кроп нужен, чтобы кадр сел в экран
/// «по заполнению», как это делал `force_original_aspect_ratio=increase`: сперва
/// от исходника отрезается лишнее по длинной стороне, потом остаток ужимается
/// ровно в цель. Числа кропа чётные — иначе не поделится цветоразностный канал.
fn cuvid_аргументы(path: &str, цель: (u32, u32)) -> Option<(String, String, String)> {
    let (tw, th) = цель;
    let (sw, sh) = размер_видео(path)?;
    if tw == 0 || th == 0 || sw == 0 || sh == 0 {
        return None;
    }
    let кодек = кодек_видео(path)?;
    if !cuvid_есть(&кодек) {
        return None;
    }
    let (сверху, слева) = cuvid_кроп((sw, sh), цель);
    Some((
        format!("{кодек}_cuvid"),
        // Порядок в ffmpeg: top x bottom x left x right.
        format!("{сверху}x{сверху}x{слева}x{слева}"),
        format!("{tw}x{th}"),
    ))
}

/// Сколько отрезать от исходника, чтобы остаток был ровно в пропорции экрана:
/// возвращает (сверху и снизу, слева и справа). Одно из двух чисел всегда ноль
/// — режем только по одной оси.
fn cuvid_кроп(исходник: (u32, u32), цель: (u32, u32)) -> (u32, u32) {
    let ((sw, sh), (tw, th)) = (исходник, цель);
    let чёт = |v: u32| v & !1;
    if (sw as u64) * (th as u64) > (tw as u64) * (sh as u64) {
        // Исходник шире цели — режем по бокам.
        let нужно = чёт(((sh as u64 * tw as u64) / th as u64) as u32).min(sw);
        (0, чёт((sw - нужно) / 2))
    } else {
        // Исходник выше цели — режем сверху и снизу.
        let нужно = чёт(((sw as u64 * th as u64) / tw as u64) as u32).min(sh);
        (чёт((sh - нужно) / 2), 0)
    }
}

/// Запустить ffmpeg на живые обои. `hw` — декодировать видеокартой.
/// Размер картинки в видеофайле. Нужен, чтобы не поручать ffmpeg то, что он
/// делать не должен, — растягивание (см. [`start_anim`]).
fn размер_видео(path: &str) -> Option<(u32, u32)> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-of", "csv=p=0:s=x",
            path,
        ])
        .output()
        .ok()?;
    let текст = String::from_utf8_lossy(&out.stdout);
    let (w, h) = текст.trim().split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Собственная частота кадров ролика.
///
/// ffprobe отдаёт её дробью («60000/1001» — это 59.94), поэтому делим сами.
/// Берём r_frame_rate: у VFR-роликов это ВЕРХНЯЯ частота, то есть темп, в
/// котором ffmpeg отдаёт кадры после фильтра `fps`.
fn частота_видео(path: &str) -> Option<f64> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=r_frame_rate",
            "-of", "csv=p=0",
            path,
        ])
        .output()
        .ok()?;
    let текст = String::from_utf8_lossy(&out.stdout);
    let текст = текст.trim();
    let (числитель, знаменатель) = текст.split_once('/').unwrap_or((текст, "1"));
    let (ч, з) = (числитель.parse::<f64>().ok()?, знаменатель.parse::<f64>().ok()?);
    // 1000 к/с в файле — это не обои, а испорченные метаданные; такому кадру
    // мы всё равно не успеем ни декодироваться, ни доехать до экрана.
    (з > 0.0 && ч > 0.0).then(|| (ч / з).clamp(1.0, 240.0))
}

/// Потолок частоты кадров живых обоев, если он вообще нужен.
///
/// По умолчанию потолка НЕТ: обои идут с той же частотой, что и файл. Если
/// живые обои на 60 к/с окажутся дороже, чем хочется, `DWALL_FPS=30` вернёт
/// прежнее поведение — ffmpeg прорядит кадры ещё до масштабирования, то есть
/// сэкономит и на декодировании тоже.
fn потолок_fps() -> Option<f64> {
    static ПОТОЛОК: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *ПОТОЛОК.get_or_init(|| {
        std::env::var("DWALL_FPS").ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
    })
}

/// `цель` — размер, в который ffmpeg должен ужать кадр; `None` — отдавать как
/// есть, без фильтра масштабирования. `fps` — потолок частоты; `None` —
/// отдавать все кадры файла, как они в нём лежат.
fn spawn_ffmpeg(path: &str, цель: Option<(u32, u32)>, fps: Option<f64>, hw: bool) -> Option<std::process::Child> {
    let mut args: Vec<String> = vec![
        "-loglevel".into(), "error".into(), "-nostdin".into(),
    ];
    // Ужимать кадр лучше всего прямо в декодере видеокарты — тогда на CPU не
    // приезжает кадр 4K и swscale не работает вовсе (см. cuvid_аргументы).
    // Не вышло (нет такого декодера, ffprobe промолчал) — обычный путь:
    // -hwaccel cuda и масштабирование фильтром.
    let cuvid = if hw { цель.and_then(|ц| cuvid_аргументы(path, ц)) } else { None };
    match &cuvid {
        Some((декодер, crop, resize)) => args.extend([
            "-c:v".to_string(), декодер.clone(),
            "-crop".into(), crop.clone(),
            "-resize".into(), resize.clone(),
        ]),
        None if hw => args.extend(["-hwaccel".to_string(), "cuda".into()]),
        None => {}
    }
    args.extend([
        "-stream_loop".to_string(), "-1".into(), "-i".into(), path.to_string(),
        "-an".into(), "-sn".into(),
        // ВАЖЕН ПОРЯДОК: `fps` (когда потолок вообще задан) стоит ПЕРВЫМ в
        // цепочке фильтров, до scale. Раньше частоту резал выходной ключ
        // `-r 15`, то есть уже ПОСЛЕ фильтров: масштабирование 4K→2560×1080
        // честно считалось на всех 60 кадрах источника, а три из четырёх
        // результатов выбрасывались. Замер на green-tree (3840×2160@60): 36.9 с
        // CPU на 12 с видео против 27.9 с после переноса fps в начало (−24%).
        //
        // Без потолка фильтра нет вовсе: `fps=<та же частота>` — это лишний
        // проход по кадру ради ничего, а прореживать нечего.
        "-vf".into(),
        {
            // Кадр, ужатый декодером, второй раз масштабировать нечем.
            let масштаб = цель.filter(|_| cuvid.is_none()).map(|(w, h)| {
                format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}")
            });
            match (fps, масштаб) {
                (Some(f), Some(s)) => format!("fps={f},{s}"),
                (Some(f), None) => format!("fps={f}"),
                (None, Some(s)) => s,
                // Пустая цепочка фильтров ffmpeg не нравится — просим копию.
                (None, None) => "null".to_string(),
            }
        },
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

/// Залить очередной кадр из трубы ffmpeg прямо в общую память слота.
///
/// `splice` перекладывает страницы трубы в memfd силами ядра — на нашей
/// стороне не остаётся ни чтения в буфер, ни копирования из него в общую
/// память. Замер 14.08.2026 на стенде (2560×1080, 120 кадров, тот же ffmpeg):
///
/// | как забираем кадр | CPU на кадр |
/// |---|---|
/// | `read_exact` в буфер + копия в общую память | 12.77 мс |
/// | `splice` прямо в memfd слота | **7.94 мс** |
///
/// Расширение самой трубы (`F_SETPIPE_SZ` до мегабайта) на том же стенде не
/// дало НИЧЕГО (9.0 против 9.5 мс на кадр) — платим не за системные вызовы, а
/// за перекладывание мегабайтов, поэтому его и не делаем.
///
/// `false` = труба кончилась (ffmpeg завершился или его убили).
fn залить_кадр(труба: &impl std::os::fd::AsRawFd, заём: &Заём, размер: usize) -> bool {
    use std::os::fd::AsRawFd;
    let mut залито = 0usize;
    while залито < размер {
        // Смещение в файле ведём сами: splice с указателем на смещение не
        // трогает позицию дескриптора, и слот можно заливать с начала снова и
        // снова.
        let mut смещение = залито as libc::loff_t;
        let n = unsafe {
            libc::splice(
                труба.as_raw_fd(), std::ptr::null_mut(),
                заём.fd.as_raw_fd(), &mut смещение,
                размер - залито, 0,
            )
        };
        match n {
            0 => return false,             // труба закрыта — ffmpeg кончился
            n if n < 0 => {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted { continue; }
                eprintln!("dwall: splice кадра не удался: {e}");
                return false;
            }
            n => залито += n as usize,
        }
    }
    true
}

/// Запускает фоновое декодирование живых обоев под размер экрана `w`×`h`.
fn start_anim(path: &str, w: u32, h: u32) -> Option<Anim> {
    use std::sync::mpsc::sync_channel;
    let (tx, rx) = sync_channel::<Кадр>(1);

    if is_video(path) {
        // Канал слотов: главный цикл кладёт сюда свободную общую память,
        // декодер заливает в неё кадр. Ёмкости не задаём — слотов всего два,
        // больше в канале и не окажется.
        let (выдать_слот, взять_слот) = std::sync::mpsc::channel::<Заём>();
        // Кадры просим в BGRA — конвертировать на нашей стороне дороже.
        // -stream_loop -1 — бесконечный повтор.
        //
        // Частота — РОДНАЯ частота файла. Раньше здесь стоял жёсткий потолок в
        // 15 к/с «обои не стоят 60 к/с процессорного времени», и он был виден
        // глазом: плавная панорама в ролике на 60 к/с шла рывками. Потолок
        // остался, но только по требованию — DWALL_FPS (см. потолок_fps).
        //
        // Декодируем видеокартой, когда она есть: 4K@60 h264 — это ~28 с CPU на
        // каждые 12 с видео (замер 05.08.2026, зелёное дерево 3840×2160),
        // против 5.7 с через NVDEC.
        let своя = частота_видео(path).unwrap_or(30.0);
        let потолок = потолок_fps().filter(|п| *п < своя);
        let fps = потолок.unwrap_or(своя);
        // Длительность кадра считаем в микросекундах: 60 к/с — это 16 666 мкс,
        // и округление до миллисекунд промахнулось бы мимо частоты экрана.
        let шаг = std::time::Duration::from_micros((1_000_000.0 / fps).round() as u64);

        // Размер, в котором просим кадры. Ужимать большой ролик в ffmpeg —
        // полезно: меньше и счёта, и данных через трубу. А вот РАСТЯГИВАТЬ там
        // мелкий ролик — чистая потеря, потому что то же самое кадрирование с
        // центрированием делает blit_scaled уже на нашей стороне.
        //
        // Замер 09.08.2026 на этих обоях (AV1 1280×720 → экран 3840×2160),
        // 120 кадров: с масштабированием в ffmpeg — 13.4 с CPU и 474 МБ/с
        // через трубу, без него — 3.8 с и 52 МБ/с. Разница в 3.5 раза, и
        // ffmpeg на живой машине держал 264% CPU при неподвижной картинке.
        // Аппаратный декодер тут не помог бы: у Comet Lake нет AV1 вовсе.
        //
        // Цена — качество растягивания: blit_scaled берёт ближайшего соседа,
        // ffmpeg брал бикубик. Ролик мельче экрана станет заметно грубее.
        let (вых_w, вых_h, цель) = match размер_видео(path) {
            Some((iw, ih)) if iw > 0 && ih > 0
                && (w as f64 / iw as f64).max(h as f64 / ih as f64) > 1.0 =>
            {
                // Исходник мельче экрана — берём как есть, растянет blit_scaled.
                (iw, ih, None)
            }
            // Исходник крупнее экрана (или ffprobe промолчал) — ужимаем в
            // ffmpeg, как и раньше.
            _ => (w, h, Some((w, h))),
        };

        let hw = cuda_available();
        let child = std::sync::Arc::new(std::sync::Mutex::new(Some(
            spawn_ffmpeg(path, цель, потолок, hw)?,
        )));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (поток_child, поток_stop) = (child.clone(), stop.clone());
        let путь = path.to_string();
        std::thread::spawn(move || {
            // Размер кадра — тот, в котором его отдаёт ffmpeg, а он теперь не
            // обязан совпадать с экраном (см. выбор `цель` выше).
            let кадр = (вых_w as usize) * (вых_h as usize) * 4;
            let mut hw = hw;
            loop {
                // Забираем stdout только что запущенного процесса.
                let out = поток_child.lock().ok().and_then(|mut g| {
                    g.as_mut().and_then(|c| c.stdout.take())
                });
                let Some(out) = out else { return };

                let mut кадров = 0u64;
                loop {
                    // Ждём, пока главный цикл отдаст свободный слот. Это и есть
                    // тормоз всего конвейера: пока показывать некуда, мы стоим
                    // здесь, труба ffmpeg переполняется, и он засыпает сам.
                    // Ошибка = главный цикл выбросил Anim, уходим.
                    let Ok(заём) = взять_слот.recv() else { return };
                    if !залить_кадр(&out, &заём, кадр) {
                        break; // ffmpeg закончился или его убили
                    }
                    кадров += 1;
                    // Свежий буфер вместо clone: read_exact всё равно перезапишет
                    // его целиком, а копия 11 МБ на кадр — это 165 МБ/с memcpy
                    // впустую.
                    // Ошибка отправки = главный цикл выбросил Anim, уходим.
                    if tx.send(Кадр::ВСлоте { слот: заём.индекс, шаг }).is_err() {
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
                    let Some(новый) = spawn_ffmpeg(&путь, цель, потолок, false) else { return };
                    match поток_child.lock() {
                        Ok(mut g) => *g = Some(новый),
                        Err(_) => return,
                    }
                    continue;
                }
                return;
            }
        });
        return Some(Anim {
            rx,
            слоты: выдать_слот,
            dims: (вых_w, вых_h),
            next_at: std::time::Instant::now(),
            child,
            stop,
        });
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
                // (так же поступают браузеры). Ничего сверх этого не
                // округляем: у гифа своя частота записана в каждом кадре, она
                // и есть исходная.
                let ms = if f.delay == 0 { 100 } else { f.delay as u64 * 10 };
                let шаг = std::time::Duration::from_millis(ms);
                if tx.send(Кадр::Буфером { w: sw as u32, h: sh as u32, px: кадр, шаг }).is_err() {
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
    // У gif своего процесса нет — убивать в Drop нечего, и слоты ему не нужны:
    // кадр он собирает у себя на холсте. Канал слотов заводим пустым, чтобы
    // поле было одно на оба вида живых обоев.
    let (выдать_слот, _) = std::sync::mpsc::channel::<Заём>();
    Some(Anim {
        rx,
        слоты: выдать_слот,
        // Слоты общей памяти gif не нужны (кадр он собирает на своём холсте),
        // поэтому размер нулевой — по нему одолжить_слоты и не берётся за дело.
        dims: (0, 0),
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
    ///
    /// Запасной путь для видео: работает везде, но платит процессором за
    /// декодирование, перевод в BGRA и перекачку кадров через трубу. Основной
    /// путь — `hw` ниже; сюда сваливаемся, когда видеокарта этот ролик не
    /// потянула. Одновременно взведён быть может только один из двух.
    anim: Option<Anim>,
    /// Живые обои, декодированные видеокартой прямо в видеопамять.
    ///
    /// Основной путь для видео. Кадр приезжает готовым dmabuf-буфером, и
    /// показать его — это прицепить `wl_buffer`: ни декодирования, ни
    /// перевода цвета, ни копирования на процессоре не остаётся вовсе.
    /// Замер 15.08.2026 на h264 2560×1440@60: 0.40 с CPU на 300 кадров против
    /// 15.7 с у пути через ffmpeg.
    hw: Option<hwvideo::HwВидео>,
    /// Готовые `wl_buffer` под поверхности VAAPI, по номеру поверхности.
    ///
    /// Пул декодера конечен и невелик (на этой машине — 5 поверхностей), а
    /// кадры идут по кругу, поэтому буферы заводятся один раз и живут до смены
    /// обоев. Флаг — тот же, что у shm-слотов: снят ли буфер с показа.
    hw_буферы: std::collections::HashMap<usize, (WlBuffer, std::sync::Arc<AtomicBool>)>,
    /// Кадры, которые композитор ещё не отпустил.
    ///
    /// Держатель не даёт декодеру забрать поверхность обратно в пул и записать
    /// поверх того, что прямо сейчас на экране. Роняем по `wl_buffer::release`.
    hw_на_экране: Vec<(std::sync::Arc<AtomicBool>, hwvideo::Держатель)>,
    /// Срок показа следующего аппаратного кадра.
    hw_срок: std::time::Instant,
    /// Глобал linux-dmabuf. Нет его — аппаратный путь недоступен, и видео
    /// поедет через `anim`.
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// Пары «формат + модификатор», которые композитор объявил поддерживаемыми.
    ///
    /// Проверять обязательно: `create_immed` на неподдерживаемом сочетании —
    /// это протокольная ошибка, то есть мгновенная смерть клиента, а не
    /// аккуратный отказ. Не нашли своё сочетание — уходим на запасной путь.
    dmabuf_форматы: std::collections::HashSet<(u32, u64)>,
    /// Обои сейчас на экране видно (композитор отвечает на кадровые запросы).
    видно: bool,
    /// Кадр живых обоев, который декодер уже залил в общую память: номер слота
    /// и его размер. Пока он взведён, render_wallpaper НЕ заполняет буфер —
    /// там уже лежит готовый кадр (см. Кадр::ВСлоте).
    кадр_слота: Option<(usize, u32, u32)>,
    /// Слоты, отданные декодеру: в них он прямо сейчас пишет, трогать нельзя.
    слоты_у_декодера: Vec<bool>,
    /// Когда заказан кадровый callback, на который ещё не ответили. Молчание
    /// дольше [`ОЖИДАНИЕ_КАДРА`] значит «обои закрыты, можно спать».
    кадр_заказан: Option<std::time::Instant>,
    /// Композитор хоть раз ответил на кадровый запрос. Пока не ответил ни разу,
    /// засыпать НЕЛЬЗЯ: на композиторе, который layer-поверхностям кадровых
    /// callback'ов не шлёт вовсе, обои иначе замерли бы навсегда.
    callback_был: bool,
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
    /// wp_viewporter, если композитор его предлагает. Нет — работаем как
    /// раньше, растягивая кадры на процессоре.
    viewporter: Option<WpViewporter>,
    /// Viewport фоновой поверхности: им задаётся, какой кусок буфера и в какой
    /// прямоугольник экрана растянуть.
    bg_viewport: Option<WpViewport>,
    /// Пользователь нажал «Поиск»: меню прячем, а строку ввода (fuzzel)
    /// запускаем из главного цикла — и только после того, как кадр со скрытым
    /// меню действительно ушёл композитору. См. ветку UiAction::Поиск.
    запросить_поиск: bool,
    /// То же для меню фильтров: его тоже показывает fuzzel, и по той же
    /// причине — своего шрифта у dwall нет.
    запросить_фильтр: bool,
    /// Чем сужена сетевая выдача (см. [`online::Фильтры`]).
    фильтры: online::Фильтры,
    /// Разделы moewalls для меню фильтров. Спрашиваются у сайта один раз за
    /// сеанс, при первом открытии меню: до него они не нужны, а сеть ждать
    /// незачем.
    разделы_живых: Vec<(u32, String)>,
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
    /// Значок загрузки: какая точка кольца сейчас самая яркая и когда её
    /// последний раз двигали. Крутится, пока идёт хоть какая-то загрузка
    /// (см. идёт_загрузка).
    спиннер_фаза: u32,
    спиннер_тик: std::time::Instant,
    /// Нарисован ли значок в последнем отданном кадре. Нужен ровно затем,
    /// чтобы стереть его ОДИН раз, когда загрузка кончилась, а не гонять
    /// перерисовку меню вхолостую.
    спиннер_виден: bool,
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

    // Служебный режим: проверить аппаратное декодирование на конкретном ролике
    // и показать, что именно отдаёт видеокарта. Композитор не нужен —
    // проверяется ровно связка libavcodec+VAAPI+dmabuf.
    if std::env::args().nth(1).as_deref() == Some("--hwtest") {
        std::process::exit(hwvideo::режим_проверки(std::env::args().nth(2)));
    }

    // Служебный режим: спросить обои у источника и показать, что нашлось.
    // Композитор не нужен — сеть проверяется отдельно от всего остального.
    if std::env::args().nth(1).as_deref() == Some("--online") {
        std::process::exit(режим_сети(
            std::env::args().nth(2),
            std::env::args().nth(3),
            std::env::args().skip(4).collect(),
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
        видно: true,
        кадр_слота: None,
        слоты_у_декодера: Vec::new(),
        кадр_заказан: None,
        callback_был: false,
        img_bgra: false,
        bg_slots: Vec::new(),
        bg_slot: 0,
        bg_dims: (0, 0),
        ol_slots: Vec::new(),
        ol_slot: 0,
        bg_pending: false,
        viewporter: None,
        bg_viewport: None,
        hw: None,
        hw_буферы: std::collections::HashMap::new(),
        hw_на_экране: Vec::new(),
        hw_срок: std::time::Instant::now(),
        dmabuf: None,
        dmabuf_форматы: std::collections::HashSet::new(),
        запросить_поиск: false,
        запросить_фильтр: false,
        фильтры: online::Фильтры::default(),
        разделы_живых: Vec::new(),
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
        спиннер_фаза: 0,
        спиннер_тик: std::time::Instant::now(),
        спиннер_виден: false,
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
                // Живые обои: спим ровно до срока СЛЕДУЮЩЕГО кадра. Раньше
                // здесь стояли фиксированные 15 мс — потолок в 66 к/с, которого
                // хватало ровно потому, что кадров и так было 15 в секунду.
                // Ролику на 60 к/с этого мало: кадр то успевал к сроку, то
                // опаздывал на целый период, и картинка дёргалась.
                //
                // Пол в 2 мс — на случай, когда срок уже прошёл, а кадра ещё
                // нет (декодер не успевает): канал не файловый дескриптор, о
                // готовности кадра poll не узнает, и остаётся опрашивать.
                // Спящим обоям частый опрос не нужен вовсе: ждём кадрового
                // callback'а из сокета, а он придёт по POLLIN.
                //
                // Срок берём у ТОГО пути, который сейчас работает: у обоев
                // через ffmpeg он свой (Anim::next_at), у аппаратных — свой
                // (hw_срок). Забыть про второй значит вернуть ровно ту дёрганую
                // картинку, о которой сказано выше: цикл засыпал на секунду и
                // просыпался только на кадровый callback, поэтому кадр то
                // успевал к сроку, то опаздывал на целый период.
                let срок_кадра = match (config.anim.as_ref(), config.hw.as_ref()) {
                    (Some(a), _) => Some(a.next_at),
                    // Пока играет переход, аппаратные кадры не забираются
                    // (см. показать_hw_кадр) — и будить цикл под них незачем.
                    (None, Some(_)) if config.fade.is_none() => Some(config.hw_срок),
                    _ => None,
                };
                let до_кадра = срок_кадра.filter(|_| config.видно).map(|срок| {
                    let ост = срок.saturating_duration_since(std::time::Instant::now());
                    (ост.as_micros().div_ceil(1000) as i32).clamp(2, 1000)
                });
                // bg_pending и переход — это не украшение: флаг значит, что
                // кадр обоев НЕ нарисован — оба буфера были у композитора (см.
                // render_wallpaper). Повторную попытку делает главный цикл, но
                // без частого опроса он засыпал на секунду, и смена обоев
                // «иногда» (когда именно в этот момент буфер был занят)
                // показывалась с задержкой до 1000 мс.
                let частые = [
                    до_кадра,
                    (идёт_переход || config.bg_pending).then_some(15),
                    // Значок загрузки крутится, пока идёт загрузка (см.
                    // ШАГ_СПИННЕРА).
                    идёт_загрузка(&config).then(|| ШАГ_СПИННЕРА.as_millis() as i32),
                    (config.pending_img.is_some() || config.pending_thumbs.is_some())
                        .then_some(30),
                ];
                let таймаут = частые.into_iter().flatten().min().unwrap_or(1000);
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

        // Видно ли обои. Композитор молчит на кадровый запрос дольше срока —
        // значит их целиком закрыли (полноэкранная игра, окна во весь экран), и
        // считать кадры незачем.
        if !config.callback_был {
            // Ни одного ответа за всю жизнь — этот композитор кадровых
            // callback'ов слоям не шлёт. Работаем по таймеру, как раньше.
        } else if let Some(заказан) = config.кадр_заказан {
            if config.видно && заказан.elapsed() >= ОЖИДАНИЕ_КАДРА {
                config.видно = false;
                // Кадры больше не забираем: очередь декодера ёмкостью 1
                // переполнится, ffmpeg упрётся в трубу и встанет сам.
                eprintln!("dwall: обои закрыты — засыпаем");
            }
        }

        // Живые обои: сначала отдаём декодеру всё, во что он может писать.
        одолжить_слоты(&mut config);

        // Живые обои: забираем очередной кадр, когда подошёл его срок.
        let видно = config.видно;
        if let Some(anim) = config.anim.as_mut().filter(|_| видно) {
            let сейчас = std::time::Instant::now();
            if сейчас >= anim.next_at {
                match anim.rx.try_recv() {
                    Ok(кадр) => {
                        let шаг = match &кадр {
                            Кадр::ВСлоте { шаг, .. } => *шаг,
                            Кадр::Буфером { шаг, .. } => *шаг,
                        };
                        // Срок следующего кадра отсчитываем от СРОКА текущего, а
                        // не от «сейчас»: иначе к каждому кадру прибавлялось бы
                        // опоздание главного цикла, и ролик на 60 к/с полз бы
                        // медленнее реального времени. Если отстали больше чем
                        // на кадр (тяжёлая перерисовка, машина занята) —
                        // догонять нечем, начинаем отсчёт заново.
                        anim.next_at = if сейчас.duration_since(anim.next_at) > шаг {
                            сейчас + шаг
                        } else {
                            anim.next_at + шаг
                        };
                        config.img_bgra = true; // и видео, и gif отдают BGRA
                        match кадр {
                            // Кадр уже в общей памяти — заполнять нечего,
                            // остаётся показать (см. render_wallpaper).
                            Кадр::ВСлоте { слот, .. } => {
                                let (dw, dh) = config.anim.as_ref().map(|a| a.dims).unwrap_or((0, 0));
                                if let Some(занят) = config.слоты_у_декодера.get_mut(слот) {
                                    *занят = false;
                                }
                                config.кадр_слота = Some((слот, dw, dh));
                            }
                            Кадр::Буфером { w, h, px, .. } => {
                                config.кадр_слота = None;
                                config.img = Some((w, h, px));
                            }
                        }
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

        // Живые обои на видеокарте: кадр уже лежит в видеопамяти готовым, и
        // весь показ — это прицепить wl_buffer.
        //
        // Пока играет плавный переход, кадры отсюда НЕ забираем: смешивать
        // dmabuf нечем, и на экране в это время неподвижный первый кадр ролика
        // обычным путём через общую память (см. установить_обои). Декодер всё
        // это время стоит на `send` и не тратит ничего.
        if config.видно && config.fade.is_none() && config.hw.is_some() {
            показать_hw_кадр(&mut config);
        }

        // Значок загрузки. Крутим его, пока идёт фоновая работа, и один раз
        // стираем, когда она кончилась; сама перерисовка — общая, через dirty
        // (меню открыто — рисуется в его ряду кнопок, закрыто — плашкой в
        // углу, см. hide_overlay).
        {
            let грузим = идёт_загрузка(&config);
            if грузим {
                if config.спиннер_тик.elapsed() >= ШАГ_СПИННЕРА {
                    config.спиннер_фаза = config.спиннер_фаза.wrapping_add(1);
                    config.спиннер_тик = std::time::Instant::now();
                    config.спиннер_виден = true;
                    config.dirty = true;
                }
            } else if config.спиннер_виден {
                config.спиннер_виден = false;
                config.dirty = true;
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

        // ── Строка поиска ────────────────────────────────────────────────────
        // Запускаем ТОЛЬКО когда кадр со скрытым меню уже ушёл (dirty снят):
        // меню лежит на слое Overlay, а fuzzel открывается на top, и стоит
        // поспешить — строка ввода окажется под меню. Проверка `!dirty`
        // заменяет ожидание: главный цикл сам повторяет попытку каждый
        // проход, попутно разбирая события, которыми композитор возвращает
        // буферы.
        if config.запросить_поиск && !config.dirty {
            config.запросить_поиск = false;
            let _ = conn.flush();

            let ответ = спросить_запрос(&config.запрос);

            config.overlay_visible = true;
            if let Some(новый) = ответ {
                config.запрос = новый;
                // Искать по своим обоям нечего — их пять штук и они все на
                // виду. Поэтому поиск всегда уводит в сеть: с «моих»
                // переходим на картинки.
                if config.вкладка == Вкладка::Мои {
                    config.вкладка = Вкладка::Картинки;
                }
                config.находки.clear();
                config.сетевые_миниатюры.clear();
                config.scroll_row = 0;
                загрузить_страницу(&mut config, 1, false);
            }
            build_ui_layout(&mut config);
            config.dirty = true;
        }

        // ── Меню фильтров ────────────────────────────────────────────────────
        // Условие то же, что у поиска: кадр со скрытым меню обязан уйти раньше,
        // чем откроется fuzzel, иначе окно fuzzel окажется под меню.
        if config.запросить_фильтр && !config.dirty {
            config.запросить_фильтр = false;
            let _ = conn.flush();

            if let Some(источник) = config.вкладка.источник() {
                // Разделы moewalls спрашиваем один раз за сеанс и только здесь:
                // до открытия меню они не нужны.
                if источник == online::Источник::Живые && config.разделы_живых.is_empty() {
                    config.разделы_живых = online::разделы_живых();
                }
                let пункты = online::меню_фильтров(
                    источник,
                    &config.фильтры,
                    &config.разделы_живых,
                    !config.запрос.is_empty(),
                );
                let подписи: Vec<String> = пункты.iter().map(|(п, _)| п.clone()).collect();
                let выбор = спросить_из_списка("Фильтр: ", &подписи);

                config.overlay_visible = true;
                if let Some(i) = выбор {
                    if let Some((_, новые)) = пункты.get(i) {
                        // Молча ничего не делаем, когда выбрали уже
                        // действующее: перезапрашивать сеть незачем.
                        if *новые != config.фильтры {
                            config.фильтры = новые.clone();
                            config.scroll_row = 0;
                            загрузить_страницу(&mut config, 1, false);
                        }
                    }
                }
            } else {
                config.overlay_visible = true;
            }
            build_ui_layout(&mut config);
            config.dirty = true;
        }
    }
}

/// `dwall --online картинки|живые [запрос] [скачать] [фильтр=значение…]` —
/// проверить источник.
///
/// Без композитора: показывает, что нашлось на первой странице, а со словом
/// «скачать» ещё и тянет первую находку. Нужен для проверки — сеть отказывает
/// иначе, чем графика, и мешать эти два вида отказов в одном месте не стоит.
///
/// Фильтры задаются теми же словами, что стоят в меню:
/// `размер=любые`, `ориентация=любая`, `род=рисунки`, `раздел=аниме`,
/// `порядок=совпадение`. Иначе проверить их можно было бы только руками через
/// меню, то есть только в живом сеансе.
fn режим_сети(какие: Option<String>, запрос: Option<String>, хвост: Vec<String>) -> i32 {
    let источник = match какие.as_deref() {
        Some("картинки") | Some("images") => online::Источник::Картинки,
        Some("живые") | Some("live") => online::Источник::Живые,
        _ => {
            eprintln!("dwall --online картинки|живые [запрос] [скачать] [фильтр=значение…]");
            return 2;
        }
    };

    let запрос = запрос.unwrap_or_default();
    let действие = хвост.iter().find(|a| a.as_str() == "скачать").cloned();
    let mut фильтры = online::Фильтры::default();
    for правка in хвост.iter().filter(|a| a.contains('=')) {
        let (ключ, значение) = правка.split_once('=').unwrap_or((правка, ""));
        let понято = match (ключ, значение) {
            ("размер", "крупные") => { фильтры.размер = online::Размер::Крупные; true }
            ("размер", "средние") => { фильтры.размер = online::Размер::Средние; true }
            ("размер", "любые") => { фильтры.размер = online::Размер::Любые; true }
            ("ориентация", "широкие") => { фильтры.ориентация = online::Ориентация::Широкие; true }
            ("ориентация", "любая") => { фильтры.ориентация = online::Ориентация::Любая; true }
            ("род", "любой") => { фильтры.род = online::Род::Любой; true }
            ("род", "рисунки") => { фильтры.род = online::Род::Рисунки; true }
            ("род", "фотографии") => { фильтры.род = online::Род::Фотографии; true }
            ("порядок", "свежие") => { фильтры.порядок = online::Порядок::Свежие; true }
            ("порядок", "совпадение") => { фильтры.порядок = online::Порядок::Совпадение; true }
            ("цвет", "любой") => { фильтры.цвет = None; true }
            ("цвет", имя) => match palette::Цвет::из_имени(имя) {
                Some(ц) => { фильтры.цвет = Some(ц); true }
                None => false,
            },
            ("раздел", "все") => { фильтры.раздел = None; true }
            ("раздел", имя) => {
                // Номер раздела знает только сайт — спрашиваем его так же,
                // как это делает меню.
                match online::разделы_живых()
                    .into_iter()
                    .find(|(_, н)| н.eq_ignore_ascii_case(имя))
                {
                    Some(раздел) => { фильтры.раздел = Some(раздел); true }
                    None => false,
                }
            }
            _ => false,
        };
        if !понято {
            eprintln!("непонятный фильтр: {правка}");
            return 2;
        }
    }

    let находки = match online::список(источник, 1, 2560, 1080, &запрос, &фильтры) {
        Ok(н) => н,
        Err(e) => {
            eprintln!("не вышло: {e}");
            return 1;
        }
    };

    // Отбор по цвету идёт по миниатюрам, поэтому здесь их приходится скачать —
    // в живом меню они и так качаются для сетки.
    let (находки, было) = match фильтры.цвет {
        None => { let n = находки.len(); (находки, n) }
        Some(цвет) => {
            let было = находки.len();
            let мини: Vec<_> = находки
                .iter()
                .map(|н| match online::миниатюра(н) {
                    Some(байты) => разобрать_миниатюру(&байты),
                    None => пустая_миниатюра(),
                })
                .collect();
            for (н, (_, _, px)) in находки.iter().zip(&мини) {
                let имя: String = н.имя.chars().take(50).collect();
                match palette::приметы_цвета(px) {
                    Some((тон, цветность, светлота)) => println!(
                        "  {имя:<50} тон {тон:>5.0}°  цветность {цветность:.2}  светлота {светлота:.2}{}",
                        if palette::подходит_по_приметам(тон, цветность, светлота, цвет) { "  ✓" } else { "" },
                    ),
                    None => println!("  {имя:<50} миниатюра не скачалась"),
                }
            }
            let (н, _) = отобрать_по_цвету(находки, мини, Some(цвет));
            (н, было)
        }
    };

    println!("найдено: {}{}{}", находки.len(),
             if запрос.is_empty() { String::new() } else { format!(" по запросу «{запрос}»") },
             match фильтры.цвет {
                 Some(ц) => format!(" (по цвету «{}» из {было})", ц.имя()),
                 None => String::new(),
             });
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
                // Необязателен: под композитором без него dwall просто
                // масштабирует кадры сам, как раньше (см. render_wallpaper).
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, version, qh, ())),
                // Тоже необязателен: нет dmabuf — видео поедет через ffmpeg.
                //
                // Версию берём не выше 3 СОЗНАТЕЛЬНО. С третьей приходят
                // события `modifier` со списком поддерживаемых сочетаний
                // «формат+модификатор», а нам этот список нужен: создавать
                // буфер вслепую нельзя (см. dmabuf_форматы). Четвёртая версия
                // заменяет их на feedback с отдельным объектом — лишняя
                // сложность ради тех же сведений.
                "zwp_linux_dmabuf_v1" if version >= 3 => {
                    state.dmabuf = Some(registry.bind(name, version.min(3), qh, ()));
                }
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
                const BTN_LEFT: u32 = 0x110;
                const BTN_RIGHT: u32 = 0x111;
                // Меню слушает ровно две кнопки. Раньше условие было «любая
                // нажатая», и боковые кнопки мыши (0x113/0x114 — «вперёд» и
                // «назад», их легко задеть ладонью) молча выбирали обои и
                // закрывали меню.
                if button != BTN_LEFT && button != BTN_RIGHT { return; }
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
                        // Меню — слой Overlay, то есть ВЫШЕ слоя top, на
                        // котором открывается fuzzel. Значит убрать меню надо
                        // ДО запуска строки ввода, иначе она откроется под ним
                        // и снаружи это выглядит как «поиск не появился».
                        //
                        // Раньше здесь стояло `hide_overlay(state)` без
                        // проверки результата, а она возвращает false, когда
                        // оба буфера ещё у композитора. Кадр не уходил, меню
                        // оставалось на экране — и тем чаще, чем больше окон
                        // на экране, потому что буферы отпускаются медленнее.
                        //
                        // Поэтому только ПРОСИМ поиск, а запускаем его из
                        // главного цикла — там уже есть готовый механизм
                        // «повторять, пока кадр не ушёл» (см. state.dirty), и
                        // там же обрабатываются события композитора, без
                        // которых буфер никогда не освободится. Делать
                        // roundtrip прямо отсюда нельзя: мы внутри разбора
                        // очереди событий.
                        state.overlay_visible = false;
                        state.запросить_поиск = true;
                        state.dirty = true;
                    }
                    Some(UiAction::Фильтр) => {
                        // Меню фильтров показывает тот же fuzzel — и по той же
                        // причине убирается заранее (см. UiAction::Поиск).
                        state.overlay_visible = false;
                        state.запросить_фильтр = true;
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

/// Кадровый callback обоев: композитор ответил — значит обои на экране видно.
///
/// Ответ приходит только на тот запрос, что сделан в render_wallpaper. Пока
/// dawn держит обои закрытыми полноэкранным окном, ответа нет вовсе — по этому
/// молчанию главный цикл и засыпает (см. ОЖИДАНИЕ_КАДРА).
impl Dispatch<WlCallback, ()> for Config {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            state.кадр_заказан = None;
            state.callback_был = true;
            // Обои снова показались: главный цикл сам заберёт застрявший в
            // декодере кадр — срок его показа давно прошёл.
            state.видно = true;
        }
    }
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

/// Глобал linux-dmabuf: копим объявленные композитором сочетания
/// «формат + модификатор».
///
/// Версия 3 шлёт их событиями `modifier` сразу после привязки. Устаревшее
/// `format` (версия 1) игнорируем: модификатора в нём нет, а без модификатора
/// буфер всё равно не создать — у видеокарты кадр разложен плиткой, и линейным
/// его считать нельзя.
impl Dispatch<ZwpLinuxDmabufV1, ()> for Config {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } = event {
            let модификатор = ((modifier_hi as u64) << 32) | modifier_lo as u64;
            state.dmabuf_форматы.insert((format, модификатор));
        }
    }
}

/// Заготовка буфера. Событий ждать не приходится: буфер создаётся через
/// `create_immed`, то есть сразу и без ответа композитора.
impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Config {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        _: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// У viewporter и самого viewport событий нет — только запросы от нас.
impl Dispatch<WpViewporter, ()> for Config {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpViewport, ()> for Config {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
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
    // Viewport фоновой поверхности. Заводим один раз на всю жизнь поверхности;
    // прямоугольники задаются позже, на каждом кадре (см. render_wallpaper).
    if let Some(ref vp) = state.viewporter {
        state.bg_viewport = Some(vp.get_viewport(&bg, qh, ()));
    }
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

    // ── Размер буфера ───────────────────────────────────────────────────────
    // С wp_viewporter буфер равен размеру КАДРА, а не экрана: композитор сам
    // растянет его на видеокарте. Это и есть вся оптимизация. Живые обои
    // 1280×720 на экране 3840×2160 — это 3.7 МБ на кадр вместо 33 МБ, и ни
    // одного масштабирования на процессоре: мы просто копируем кадр как есть.
    //
    // Замер 09.08.2026 до правки: ffmpeg растягивал сам — 264% CPU; после
    // переноса растягивания в dwall (blit_scaled) — 21% у ffmpeg и 48% у
    // dwall. Теперь растягивать не должен никто из них.
    //
    // Без viewporter (другой композитор) всё как раньше — буфер размером с
    // экран, растягиваем сами.
    let по_viewport = state.bg_viewport.is_some();
    // Кадр, залитый декодером прямо в общую память, задаёт размер сам: своего
    // `img` у него нет и быть не должно — весь смысл в том, что кадр нигде,
    // кроме слота, не лежит.
    let (bw, bh) = match (state.кадр_слота, по_viewport, state.img.as_ref()) {
        (Some((_, dw, dh)), _, _) if dw > 0 && dh > 0 => (dw, dh),
        (_, true, Some((iw, ih, _))) if *iw > 0 && *ih > 0 => (*iw, *ih),
        _ => (w, h),
    };
    let buf_size = (bw * bh * 4) as usize;

    // ── Два постоянных буфера вместо нового на каждый кадр ──────────────────
    // Пишем всегда в тот, что не показан сейчас. Раньше каждый кадр создавал
    // memfd, mmap, пул и wl_buffer — на живых обоях это десятки раз в секунду.
    if state.bg_dims != (bw, bh) || state.bg_slots.len() < 2 {
        state.bg_slots.clear();
        let (Some(shm), Some(qh)) = (state.shm.as_ref(), state.qh.as_ref()) else { return };
        let Some(slots) = make_slots(shm, qh, bw, bh) else { return };
        state.bg_slots = slots;
        state.bg_dims = (bw, bh);
        state.bg_slot = 0;
        // Кэш растянутой картинки считался под старый размер.
        state.scaled = None;
    }

    // Статичные обои готовим ОДИН раз: растягиваем под выход и переводим в
    // BGRA. Дальше и обычная перерисовка, и каждый кадр перехода — это просто
    // копирование готового буфера.
    //
    // Считаем в локальную переменную, а не прямо в state.scaled: пока живо
    // заимствование state.img, писать в state нельзя.
    // В режиме viewport кэш не нужен вовсе: буфер равен размеру картинки, и
    // «растягивание» сводится к копированию.
    let новый_кэш = if !по_viewport && state.anim.is_none() && state.scaled.is_none() {
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
    // Кадр от декодера уже лежит в своём слоте — его и показываем, ничего не
    // заполняя. Это и есть вся экономия: кадр приходит из ffmpeg сразу в ту
    // память, которую заберёт композитор.
    let готовый = state.кадр_слота.and_then(|(i, _, _)| (i < state.bg_slots.len()).then_some(i));
    let next = match готовый {
        Some(i) => i,
        None => match take_free_slot(&state.bg_slots, state.bg_slot) {
            Some(i) => i,
            None => {
                state.bg_pending = true;
                return;
            }
        },
    };
    state.bg_pending = false;
    state.bg_slot = next;
    let slot = &state.bg_slots[state.bg_slot];
    let dst = unsafe { std::slice::from_raw_parts_mut(slot.ptr, slot.len) };

    if готовый.is_some() {
        // Заполнять нечего. Дальше по коду — переход и viewport, они работают
        // поверх уже лежащего кадра.
    } else {
    match state.img {
        // Статичные обои идут через кэш: во время перехода кадр собирается
        // заново каждые ~16 мс, а Lanczos на 4K столько раз не потянуть.
        Some(_) if state.anim.is_none() && state.scaled.is_some() => {
            let кэш = state.scaled.as_ref().unwrap();
            if кэш.len() >= buf_size {
                dst.copy_from_slice(&кэш[..buf_size]);
            }
        }
        // Кадр уже нужного размера — просто копируем. Ни масштабирования, ни
        // свопа: самый дешёвый путь, ради него всё. С viewporter сюда попадает
        // ЛЮБОЙ кадр, потому что буфер под него и заведён.
        Some((iw, ih, ref px)) if iw == bw && ih == bh && px.len() >= buf_size => {
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
            blit_scaled(px, iw, ih, dst, bw, bh, state.img_bgra);
        }
        // Статичная картинка — путь прежний, качественный: она масштабируется
        // один раз при выборе, экономить тут нечего.
        Some((iw, ih, ref px)) => {
            let mut rgba = resize(iw, ih, px, bw, bh);
            if !state.img_bgra {
                for c in rgba.chunks_exact_mut(4) { c.swap(0, 2); }
            }
            if rgba.len() < buf_size { return; }
            dst.copy_from_slice(&rgba[..buf_size]);
        }
        None => dst.fill(35),
    }
    }

    // ── Плавный переход ─────────────────────────────────────────────────────
    // В dst уже лежит НОВЫЙ кадр; подмешиваем к нему снимок старого. Смешиваем
    // прямо на месте — лишний буфер на весь экран (это десятки мегабайт) тут
    // ни к чему.
    if let Some(fade) = state.fade.as_mut() {
        // Снимок сделан под ПРОШЛЫЕ обои, а у новых свой размер кадра (с
        // viewporter это обычное дело). Подгоняем один раз за переход, а не на
        // каждом кадре: дальше размеры уже совпадут.
        if !fade.ждём && fade.dims != (bw, bh) && fade.dims.0 > 0 && fade.dims.1 > 0 {
            let mut подогнан = vec![0u8; buf_size];
            blit_scaled(
                &fade.from, fade.dims.0, fade.dims.1,
                &mut подогнан, bw, bh,
                true, // в буферах уже BGRA, менять местами нечего
            );
            fade.from = подогнан;
            fade.dims = (bw, bh);
        }
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

    // ── Viewport: какой кусок буфера и куда растянуть ───────────────────────
    // Задаём тот же закон, что раньше исполнял blit_scaled: «заполнить» —
    // масштаб по большей стороне, лишнее обрезаем поровну с двух краёв.
    // Считает это теперь композитор на видеокарте, и он же фильтрует, поэтому
    // «лесенки» от ближайшего соседа больше нет.
    //
    // set_source задаётся в координатах БУФЕРА и принимает дробные числа,
    // set_destination — в логических пикселях экрана.
    if let Some(vp) = state.bg_viewport.as_ref() {
        let k = (w as f64 / bw as f64).max(h as f64 / bh as f64);
        let вид_w = (w as f64 / k).min(bw as f64).max(1.0);
        let вид_h = (h as f64 / k).min(bh as f64).max(1.0);
        let off_x = (bw as f64 - вид_w) / 2.0;
        let off_y = (bh as f64 - вид_h) / 2.0;
        vp.set_source(off_x, off_y, вид_w, вид_h);
        vp.set_destination(w as i32, h as i32);
    }

    let Some(surface) = state.bg_surface.as_ref() else { return };
    let slot = &state.bg_slots[state.bg_slot];
    slot.free.store(false, Ordering::Relaxed);
    // Просим кадровый callback — по нему и понимаем, видно ли обои вообще.
    //
    // Композитор не обязан отвечать: dawn молчит, пока обои целиком накрыты
    // полноэкранным окном (см. Dawn::wallpaper_hidden). Молчание дольше
    // ЗАСЫПАНИЕ и есть «нас не видно»: главный цикл перестаёт забирать кадры,
    // очередь декодера (ёмкостью 1) переполняется, ffmpeg упирается в трубу и
    // встаёт. Запрос при этом не пропадает — он ждёт в поверхности и приедет
    // первым же кадром, когда обои снова покажутся.
    if let Some(qh) = state.qh.as_ref() {
        surface.frame(qh, ());
        state.кадр_заказан = Some(std::time::Instant::now());
    }
    surface.attach(Some(&slot.buffer), 0, 0);
    // Порча задаётся в координатах БУФЕРА, а он теперь размером с кадр, а не с
    // экран: damage_buffer(0,0,w,h) с экранными числами на маленьком буфере
    // просто выходит за его границы.
    surface.damage_buffer(0, 0, bw as i32, bh as i32);
    surface.commit();
}

/// Завести `wl_buffer` поверх готовых dmabuf-дескрипторов.
///
/// `None` — композитор такое сочетание формата с модификатором не объявлял, и
/// создавать буфер НЕЛЬЗЯ: `create_immed` в этом случае отвечает не отказом, а
/// протокольной ошибкой, то есть мгновенно убивает клиента.
fn создать_dmabuf_буфер(
    state: &Config,
    описание: &hwvideo::ОписаниеПоверхности,
) -> Option<(WlBuffer, std::sync::Arc<AtomicBool>)> {
    let dmabuf = state.dmabuf.as_ref()?;
    let qh = state.qh.as_ref()?;

    if !state.dmabuf_форматы.contains(&(описание.fourcc, описание.модификатор)) {
        eprintln!(
            "dwall: композитор не объявил формат {:?} с модификатором {:#x}",
            std::str::from_utf8(&описание.fourcc.to_le_bytes()).unwrap_or("?").trim_end(),
            описание.модификатор,
        );
        return None;
    }

    let params = dmabuf.create_params(qh, ());
    let (hi, lo) = (
        (описание.модификатор >> 32) as u32,
        (описание.модификатор & 0xffff_ffff) as u32,
    );
    for (номер, плоскость) in описание.плоскости.iter().enumerate() {
        params.add(
            плоскость.fd.as_fd(),
            номер as u32,
            плоскость.сдвиг,
            плоскость.шаг,
            hi,
            lo,
        );
    }

    // create_immed, а не create: буфер нужен здесь и сейчас, а ждать ответа
    // композитора в главном цикле — это лишний круг диспетчеризации на каждую
    // новую поверхность. Сочетание формата с модификатором мы уже проверили
    // выше, так что протокольной ошибке взяться неоткуда.
    let свободен = std::sync::Arc::new(AtomicBool::new(true));
    let буфер = params.create_immed(
        описание.ширина as i32,
        описание.высота as i32,
        описание.fourcc,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        свободен.clone(),
    );
    // Заготовка своё отработала: буфер от неё уже не зависит.
    params.destroy();

    Some((буфер, свободен))
}

/// Показать очередной кадр аппаратных живых обоев.
///
/// Здесь нет ни единого касания пикселей: кадр приехал готовым буфером в
/// видеопамяти, и вся работа — прицепить его к поверхности.
fn показать_hw_кадр(state: &mut Config) {
    // Кадры, которые композитор снял с показа, отпускаем: пока держатель жив,
    // декодер не может забрать поверхность обратно в пул.
    state.hw_на_экране.retain(|(свободен, _)| !свободен.load(Ordering::Relaxed));

    // Пока не подтверждён первый configure, attach запрещён протоколом.
    if !state.bg_configured {
        return;
    }
    let сейчас = std::time::Instant::now();
    if сейчас < state.hw_срок {
        return;
    }

    // Забираем кадр. Первый кадр забран ещё при запуске декодера — там по нему
    // проверялось, что видеокарта вообще потянула этот кодек (см. hwvideo.rs),
    // и показать его надо прежде всего остального.
    let первый = state.hw.as_mut().and_then(|hw| hw.первый.take());
    let кадр = match первый {
        Some(кадр) => кадр,
        None => match state.hw.as_ref().map(|hw| hw.rx.try_recv()) {
            Some(Ok(кадр)) => кадр,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return,
            // Поток декодера кончился (битый файл, карта отказала на середине).
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                state.hw = None;
                return;
            }
            None => return,
        },
    };

    // Срок следующего кадра отсчитываем от срока текущего, а не от «сейчас»:
    // иначе к каждому кадру прибавлялось бы опоздание главного цикла, и ролик
    // на 60 к/с полз бы медленнее реального времени. Отстали больше чем на
    // кадр — догонять нечем, начинаем отсчёт заново.
    state.hw_срок = if сейчас.duration_since(state.hw_срок) > кадр.шаг {
        сейчас + кадр.шаг
    } else {
        state.hw_срок + кадр.шаг
    };

    // Поверхность, которую видим впервые, — заводим под неё буфер. Дальше она
    // будет узнаваться по номеру, и протокольной работы не останется вовсе.
    if let Some(описание) = кадр.описание.as_ref() {
        match создать_dmabuf_буфер(state, описание) {
            Some(пара) => {
                state.hw_буферы.insert(описание.surface_id, пара);
            }
            None => {
                // Композитор такого не примет — уходим на путь через ffmpeg.
                eprintln!("dwall: dmabuf не принят композитором, откат на ffmpeg");
                state.hw = None;
                state.hw_на_экране.clear();
                let (w, h) = state.outputs.first().map(|(_, w, h)| (*w, *h)).unwrap_or((0, 0));
                let путь = state.wallpapers.get(state.current).cloned();
                if let (Some(путь), true) = (путь, w > 0 && h > 0) {
                    state.anim = start_anim(&путь, w, h);
                }
                return;
            }
        }
    }

    let Some((буфер, свободен)) = state.hw_буферы.get(&кадр.surface_id) else { return };
    let (буфер, свободен) = (буфер.clone(), свободен.clone());

    // ── Куда растянуть ──────────────────────────────────────────────────────
    // Тот же закон «заполнить», что и у обычного пути: масштаб по большей
    // стороне, лишнее срезаем поровну с краёв. Считает это композитор на
    // видеокарте, поэтому кадр 2560×1440 честно фильтруется до 3840×2160, а не
    // растягивается ближайшим соседом.
    let (эw, эh) = match state.outputs.first() {
        Some((_, w, h)) if *w > 0 && *h > 0 => (*w, *h),
        _ => return,
    };
    let (кw, кh) = state.hw.as_ref().map(|hw| hw.размер).unwrap_or((эw, эh));
    if кw == 0 || кh == 0 {
        return;
    }
    if let Some(vp) = state.bg_viewport.as_ref() {
        let k = (эw as f64 / кw as f64).max(эh as f64 / кh as f64);
        let вид_w = (эw as f64 / k).min(кw as f64).max(1.0);
        let вид_h = (эh as f64 / k).min(кh as f64).max(1.0);
        vp.set_source((кw as f64 - вид_w) / 2.0, (кh as f64 - вид_h) / 2.0, вид_w, вид_h);
        vp.set_destination(эw as i32, эh as i32);
    }

    let Some(surface) = state.bg_surface.as_ref() else { return };
    // Кадровый callback — по нему же, что и на обычном пути, понимаем, видно ли
    // обои вообще. Композитор молчит, пока они закрыты полноэкранным окном;
    // главный цикл тогда перестаёт забирать кадры, декодер встаёт на `send`, и
    // видеокарта тоже отдыхает.
    if let Some(qh) = state.qh.as_ref() {
        surface.frame(qh, ());
        state.кадр_заказан = Some(сейчас);
    }
    свободен.store(false, Ordering::Relaxed);
    surface.attach(Some(&буфер), 0, 0);
    // Порча — в координатах буфера, а он размером с кадр, не с экран.
    surface.damage_buffer(0, 0, кw as i32, кh as i32);
    surface.commit();

    // Держим поверхность занятой, пока композитор не пришлёт release.
    state.hw_на_экране.push((свободен, кадр.держатель));
}

/// Отдать декодеру видео свободные слоты общей памяти.
///
/// Он пишет кадр прямо в них (см. [`залить_кадр`]), поэтому слот можно
/// одалживать, только когда его отпустил композитор (`free`) и когда он не
/// показан прямо сейчас. Пока обои закрыты полноэкранным окном, не одалживаем
/// вовсе — декодер встаёт на `recv`, ffmpeg упирается в трубу и засыпает.
///
/// Слоты заводятся здесь же, по размеру кадра из [`Anim::dims`]: декодеру они
/// нужны ДО первого кадра, а render_wallpaper узнаёт размер только из готового.
fn одолжить_слоты(state: &mut Config) {
    let Some((dw, dh)) = state.anim.as_ref().map(|a| a.dims) else { return };
    if dw == 0 || dh == 0 || !state.видно || state.bg_viewport.is_none() {
        // Без viewporter кадр надо ещё растянуть под экран, то есть через
        // буфер — тогда работает прежний путь с копированием.
        return;
    }
    if state.bg_dims != (dw, dh) || state.bg_slots.len() < 2 {
        let (Some(shm), Some(qh)) = (state.shm.as_ref(), state.qh.as_ref()) else { return };
        let Some(slots) = make_slots(shm, qh, dw, dh) else { return };
        state.bg_slots = slots;
        state.bg_dims = (dw, dh);
        state.bg_slot = 0;
        state.кадр_слота = None;
        state.scaled = None;
        state.слоты_у_декодера = vec![false; state.bg_slots.len()];
    }
    if state.слоты_у_декодера.len() != state.bg_slots.len() {
        state.слоты_у_декодера = vec![false; state.bg_slots.len()];
    }

    let показан = state.bg_slot;
    let есть_кадр = state.кадр_слота.map(|(i, _, _)| i);
    for i in 0..state.bg_slots.len() {
        if state.слоты_у_декодера[i] || Some(i) == есть_кадр {
            continue;
        }
        // Показанный слот держит композитор — он же снимет `free` при release.
        if i == показан && !state.bg_slots[i].free.load(Ordering::Relaxed) {
            continue;
        }
        if !state.bg_slots[i].free.load(Ordering::Relaxed) {
            continue;
        }
        let Ok(fd) = state.bg_slots[i].fd.try_clone() else { continue };
        let Some(anim) = state.anim.as_ref() else { return };
        if anim.слоты.send(Заём { индекс: i, fd }).is_ok() {
            state.слоты_у_декодера[i] = true;
        }
    }
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
    // Первый запуск: буферов ещё нет, показывать переход не от чего.
    //
    // Раньше здесь стояла ещё и проверка bg_dims == (w, h). С viewporter она
    // отключала бы переход НАВСЕГДА: буфер теперь равен размеру кадра, а не
    // экрана, и совпадать с ним не обязан. Снимок берём какой есть и запоминаем
    // его размер — подгонит его render_wallpaper, один раз за переход.
    if w == 0 || h == 0 || state.bg_slots.is_empty() {
        state.fade = None;
        return;
    }
    let Some(slot) = state.bg_slots.get(state.bg_slot) else { return };
    let текущий = unsafe { std::slice::from_raw_parts(slot.ptr, slot.len) }.to_vec();
    state.fade = Some(Fade {
        from: текущий,
        dims: state.bg_dims,
        start: None,
        ждём: true,
    });
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
    // До первого ack прикреплять буфер запрещено протоколом — за это
    // композитор убивает клиента («must ack the initial configure before
    // attaching buffer»), и dwall уходит в цикл перезапусков.
    //
    // Проверка стояла только в render_overlay, а сюда ведёт вторая дорога:
    // значок загрузки при ЗАКРЫТОМ меню (см. плашка_загрузки ниже). Пока
    // фоновая работа при старте была только у статичных обоев, поймать это
    // было почти нечем — картинка успевала загрузиться позже первого
    // configure. С живыми обоями окно стало шире: у них в фоне считается кадр
    // для плавного перехода, и спиннер зажигается в первые же миллисекунды,
    // ещё до ответа композитора.
    if !state.ol_configured { return false; }
    let Some(idx) = overlay_slot(state) else { return false };
    let Some(compositor) = state.compositor.clone() else { return false };
    let Some(qh) = state.qh.clone() else { return false };
    let Some(surface) = state.ol_surface.clone() else { return false };

    // Пустой регион ввода: клики проходят насквозь.
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    // Меню спрятано, но загрузка идти может — тогда в пустом кадре остаётся
    // один значок. Иначе после Win+W на тяжёлых обоях (4K-PNG декодируется
    // сотни миллисекунд) экран просто ничего не отвечал: непонятно, поймало
    // нажатие или нет.
    let значок = state.спиннер_виден.then(|| плашка_загрузки(state.спиннер_фаза));

    let slot = &state.ol_slots[idx];
    match &значок {
        Some(pixels) => unsafe {
            let буфер = std::slice::from_raw_parts_mut(slot.ptr, slot.len);
            let сколько = pixels.len().min(буфер.len());
            буфер[..сколько].copy_from_slice(&pixels[..сколько]);
            буфер[сколько..].fill(0);
        },
        None => unsafe { std::slice::from_raw_parts_mut(slot.ptr, slot.len).fill(0) },
    }
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

    // Кнопка фильтров — только на сетевых вкладках: свои обои сужать нечем.
    // Подсвечена, когда выдача чем-то сужена, — тем же приёмом, что и поиск.
    if state.вкладка != Вкладка::Мои {
        let (fx, fy) = filter_rect();
        let сужено = state.фильтры.заданы();
        let фон = if сужено {
            HOVER_COLOR
        } else if is_hover(state, fx, fy, ARROW, ARROW) {
            PLUS_COLOR
        } else {
            CARD_COLOR
        };
        fill_rounded_rect(&mut pixels, UI_W, UI_H, fx, fy, ARROW, ARROW, 6, фон);
        draw_filter_icon(&mut pixels, fx, fy, if сужено { 0xFFEEEEEEu32 } else { 0xFF888888 });
        if сужено {
            fill_rounded_rect(&mut pixels, UI_W, UI_H, fx, fy + ARROW + 3, ARROW, 2, 1, SEL_COLOR);
        }
    }

    // Значок загрузки — в ряду кнопок, за последней из них. Заводится и гаснет
    // главным циклом (см. спиннер_виден), здесь только рисуем.
    if state.спиннер_виден {
        let (sx, sy) = спиннер_место(state.вкладка);
        нарисовать_спиннер(&mut pixels, UI_W, UI_H, sx, sy, ARROW, state.спиннер_фаза);
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

/// Показать список и вернуть НОМЕР выбранной строки.
///
/// Тот же fuzzel, что и для поиска, только теперь в обычном своём качестве —
/// выбором из списка. Номер, а не текст: подписи содержат галочку действующего
/// значения и совпадать между собой могут как угодно, а состояние фильтров
/// вызывающий держит рядом со списком (см. [`online::меню_фильтров`]).
fn спросить_из_списка(подпись: &str, строки: &[String]) -> Option<usize> {
    let bin = which("fuzzel")?;
    let mut дитя = std::process::Command::new(bin)
        .args(["--dmenu", &format!("--prompt={подпись}"), "--width=36"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    {
        use std::io::Write;
        let mut вход = дитя.stdin.take()?;
        // Ошибку записи глотаем намеренно: fuzzel мог закрыться сразу (Esc), и
        // тогда труба рвётся — это не беда, а обычный отказ от выбора.
        let _ = вход.write_all(строки.join("\n").as_bytes());
    }
    let out = дитя.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let выбрано = String::from_utf8_lossy(&out.stdout).trim().to_string();
    строки.iter().position(|s| s == &выбрано)
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
    let фильтры = state.фильтры.clone();
    std::thread::spawn(move || {
        let ответ = match online::список(источник, страница, w, h, &запрос, &фильтры) {
            Err(e) => ИзСети::Ошибка(e),
            Ok(находки) => {
                // Миниатюры качаем и разбираем здесь же: главный цикл получит
                // всё готовым и ни разу не остановится ни на сети, ни на
                // декодировании.
                let мини: Vec<_> = находки
                    .iter()
                    .map(|н| match online::миниатюра(н) {
                        Some(байты) => разобрать_миниатюру(&байты),
                        None => пустая_миниатюра(),
                    })
                    .collect();
                let (находки, мини) = отобрать_по_цвету(находки, мини, фильтры.цвет);
                ИзСети::Список(находки, мини)
            }
        };
        let _ = tx.send(ответ);
    });
}

/// Оставить только те находки, чья миниатюра подходит под выбранный цвет.
///
/// Отбор именно здесь, а не в [`online`]: ни Openverse, ни moewalls цвета не
/// знают, зато миниатюры к этому месту уже скачаны и разобраны — считать по ним
/// цвет ничего не стоит, а второй раз ходить в сеть не нужно.
///
/// Страница после отбора становится короче — это нормально: докрутив сетку до
/// низа, dwall подтянет следующую (см. `прокрутить`), и так до тех пор, пока
/// подходящих не наберётся на экран.
fn отобрать_по_цвету(
    находки: Vec<online::Находка>,
    мини: Vec<(u32, u32, Vec<u8>)>,
    цвет: Option<palette::Цвет>,
) -> (Vec<online::Находка>, Vec<(u32, u32, Vec<u8>)>) {
    let Some(цвет) = цвет else { return (находки, мини) };
    находки
        .into_iter()
        .zip(мини)
        .filter(|(_, (_, _, px))| palette::подходит_по_цвету(px, цвет))
        .unzip()
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

/// Кнопка фильтров — следом за поиском. Живёт только на сетевых вкладках: у
/// своих обоев сужать нечего.
fn filter_rect() -> (u32, u32) {
    let (sx, sy) = search_rect();
    (sx + ARROW + 8, sy)
}

/// Где рисовать значок загрузки: за последней кнопкой ряда.
fn спиннер_место(вкладка: Вкладка) -> (u32, u32) {
    let (x, y) = if вкладка == Вкладка::Мои { search_rect() } else { filter_rect() };
    (x + ARROW + 10, y)
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

/// Значок фильтров — воронка: три полосы, каждая короче предыдущей, и ножка.
///
/// Форма выбрана «на отличие»: рядом уже стоят решётка, гора, треугольник и
/// лупа, а подписей в меню нет и быть не может — шрифта у dwall нет.
fn draw_filter_icon(pixels: &mut [u8], x: u32, y: u32, цвет: u32) {
    let верх = y + ARROW / 2 - 6;
    let центр = x + ARROW / 2;
    // Полосы: 14, 9 и 4 пикселя шириной, по одной через строку.
    for (i, ширина) in [14u32, 9, 4].into_iter().enumerate() {
        let py = верх + i as u32 * 4;
        fill_rounded_rect(pixels, UI_W, UI_H, центр - ширина / 2, py, ширина, 2, 1, цвет);
    }
    // Ножка вниз от самой узкой полосы — иначе воронка похожа на «список».
    fill_rounded_rect(pixels, UI_W, UI_H, центр - 1, верх + 10, 2, 4, 0, цвет);
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
    if state.вкладка != Вкладка::Мои {
        let (fx, fy) = filter_rect();
        state.hit_rects.push(HitRect { x: fx, y: fy, w: ARROW, h: ARROW, action: UiAction::Фильтр });
    }

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
    // То же для аппаратного пути. Буферы старых поверхностей уничтожаем явно:
    // прокси wayland-client в Drop ничего не разрушает, и при каждой смене
    // обоев на композиторе оседал бы лишний пул.
    state.hw = None;
    state.hw_на_экране.clear();
    for (буфер, _) in state.hw_буферы.values() {
        буфер.destroy();
    }
    state.hw_буферы.clear();

    // Живые форматы (видео и многокадровый gif) идут своим путём — кадрами.
    let (w, h) = state.outputs.first().map(|(_, w, h)| (*w, *h)).unwrap_or((0, 0));

    // ── Видео: сперва пробуем видеокарту ────────────────────────────────────
    // Кадр остаётся в видеопамяти от декодера до экрана, процессор не трогает
    // ни одного пикселя (см. hwvideo.rs). Не вышло — молча уходим на путь
    // через процесс ffmpeg ниже: обои важнее экономии.
    if w > 0 && h > 0 && is_video(&path) && state.dmabuf.is_some() {
        if let Some(hw) = hwvideo::запустить(&path, hwvideo::УЗЕЛ_ПО_УМОЛЧАНИЮ) {
            state.hw = Some(hw);
            state.hw_срок = std::time::Instant::now();
            state.кадр_слота = None;
            // Плавный переход играем по неподвижному первому кадру — сам
            // аппаратный кадр смешать не с чем. Пока он считается, декодер
            // стоит на первом же кадре и ничего не тратит.
            let (tx, rx) = std::sync::mpsc::channel();
            state.pending_img = Some(rx);
            let p = path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(кадр_для_перехода(&p));
            });
            return;
        }
        eprintln!("dwall: видеокарта этот ролик не взяла — декодируем процессором");
    }

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

// ── Значок загрузки ──────────────────────────────────────────────────────────

/// Сколько ждём ответа на кадровый запрос, прежде чем счесть обои закрытыми.
///
/// Ответ приходит с частотой монитора (≈7 мс на 144 Гц), так что треть секунды
/// — это заведомо не «композитор задумался», а «нам не отвечают вовсе». Больше
/// брать незачем: всё это время живые обои считаются впустую.
const ОЖИДАНИЕ_КАДРА: std::time::Duration = std::time::Duration::from_millis(300);

/// Как часто проворачивается значок загрузки. 90 мс на точку — это полный
/// оборот примерно за 0.7 с: видно, что жизнь идёт, и не рябит.
const ШАГ_СПИННЕРА: std::time::Duration = std::time::Duration::from_millis(90);
const ТОЧЕК_СПИННЕРА: u32 = 8;
/// Сторона квадратика со значком, когда он показывается сам по себе (меню
/// закрыто, а обои или картинка ещё грузятся).
const СПИННЕР_ПЛАШКА: u32 = 44;

/// Идёт ли прямо сейчас хоть какая-нибудь загрузка.
///
/// Всё это — фоновая работа, из-за которой на экране какое-то время ничего не
/// происходит: обои декодируются (4K-PNG — сотни миллисекунд), считаются
/// миниатюры для меню, идёт запрос или скачивание из сети.
///
/// Отдельным пунктом — живые обои, у которых ещё не пришёл ПЕРВЫЙ кадр
/// (`fade.ждём`). Через pending_img они не проходят вовсе (см.
/// start_load_wallpaper), а ждать там дольше всего: два вызова ffprobe, запуск
/// ffmpeg и раскрутка декодера.
fn идёт_загрузка(state: &Config) -> bool {
    state.pending_img.is_some()
        || state.pending_thumbs.is_some()
        || state.сетевая_работа
        || state.fade.as_ref().map(|f| f.ждём).unwrap_or(false)
}

/// Кольцо из точек, по которому бежит яркость.
///
/// Точки, а не крутящаяся дуга: рисуем без сглаживания и без блендинга (см.
/// fill_rounded_rect), и дуга из таких пикселей выглядела бы рваной лесенкой,
/// а круглая точка — нет.
fn нарисовать_спиннер(pixels: &mut [u8], sw: u32, sh: u32, x: u32, y: u32, size: u32, фаза: u32) {
    let центр = (x as f64 + size as f64 / 2.0, y as f64 + size as f64 / 2.0);
    let радиус = size as f64 / 2.0 - 3.0;
    let точка = (size as f64 / 9.0).max(1.5);
    for i in 0..ТОЧЕК_СПИННЕРА {
        let угол = std::f64::consts::TAU * i as f64 / ТОЧЕК_СПИННЕРА as f64
            - std::f64::consts::FRAC_PI_2;
        // Самая яркая — та точка, до которой дошла фаза; дальше по кругу гаснет.
        let позади = ((i + ТОЧЕК_СПИННЕРА - фаза % ТОЧЕК_СПИННЕРА) % ТОЧЕК_СПИННЕРА) as f64;
        let яркость = 1.0 - позади / ТОЧЕК_СПИННЕРА as f64;
        let a = (60.0 + 195.0 * яркость) as u32;
        // Пиксели premultiplied (так их ждёт wl_shm ARGB8888): вместе с
        // прозрачностью гасим и цвет, иначе тусклые точки светятся ореолом.
        let c = 0xDD * a / 255;
        let цвет = (a << 24) | (c << 16) | (c << 8) | c;
        нарисовать_точку(
            pixels, sw, sh,
            центр.0 + радиус * угол.cos(), центр.1 + радиус * угол.sin(),
            точка, цвет,
        );
    }
}

/// Кадр оверлея, в котором нет ничего, кроме значка загрузки в углу: так он
/// выглядит, когда меню закрыто, а обои или картинка ещё грузятся. Пиксели
/// уже в BGRA (как ждёт wl_shm).
fn плашка_загрузки(фаза: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (UI_W * UI_H * 4) as usize];
    // Плашка под значком: на светлых обоях серые точки сами по себе теряются.
    let (x, y) = (UI_W - СПИННЕР_ПЛАШКА - 4, UI_H - СПИННЕР_ПЛАШКА - 4);
    fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y,
                      СПИННЕР_ПЛАШКА, СПИННЕР_ПЛАШКА, CORNER, BG_COLOR);
    нарисовать_спиннер(&mut pixels, UI_W, UI_H, x + 8, y + 8,
                       СПИННЕР_ПЛАШКА - 16, фаза);
    for chunk in pixels.chunks_exact_mut(4) { chunk.swap(0, 2); }
    pixels
}

/// Залитый кружок с центром в дробной точке — из него собран спиннер.
fn нарисовать_точку(pixels: &mut [u8], sw: u32, sh: u32, cx: f64, cy: f64, r: f64, color: u32) {
    let rgba = [
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
        ((color >> 24) & 0xFF) as u8,
    ];
    let x0 = (cx - r).floor().max(0.0) as u32;
    let x1 = ((cx + r).ceil().max(0.0) as u32).min(sw);
    let y0 = (cy - r).floor().max(0.0) as u32;
    let y1 = ((cy + r).ceil().max(0.0) as u32).min(sh);
    for py in y0..y1 {
        for px in x0..x1 {
            let (dx, dy) = (px as f64 + 0.5 - cx, py as f64 + 0.5 - cy);
            if dx * dx + dy * dy > r * r { continue; }
            let idx = ((py * sw + px) * 4) as usize;
            if idx + 3 < pixels.len() { pixels[idx..idx + 4].copy_from_slice(&rgba); }
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

/// Первый кадр ролика в РОДНОМ разрешении — снимок, по которому играется
/// плавный переход на живые обои.
///
/// Зачем отдельно от [`first_video_frame`]: тот отдаёт миниатюру 640×360 для
/// меню, и на экране 3840×2160 переход по ней шёл бы через заметное мыло.
///
/// Аппаратный декодер смешивать нечем: его кадр лежит в видеопамяти, и
/// процессор до него не дотягивается (в этом вся суть, см. hwvideo.rs).
/// Поэтому переход играется по этому неподвижному кадру обычным путём через
/// общую память, а когда он доигран, эстафету незаметно принимает видеокарта —
/// картинка та же самая, стыка не видно.
///
/// Цена — один запуск ffmpeg на ОДИН кадр при смене обоев. На фоне того, что
/// раньше ffmpeg работал непрерывно все 60 кадров в секунду, это ничто.
fn кадр_для_перехода(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = размер_видео(path)?;
    if w == 0 || h == 0 {
        return None;
    }
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel", "error", "-nostdin",
            "-i", path, "-frames:v", "1",
            // rgba, а не bgra: так же, как отдаёт load_image, и общий путь
            // показа сам свопнет каналы (img_bgra = false).
            "-f", "rawvideo", "-pix_fmt", "rgba", "-",
        ])
        .output()
        .ok()?;
    let нужно = (w as usize) * (h as usize) * 4;
    if out.stdout.len() < нужно {
        return None;
    }
    Some((w, h, out.stdout[..нужно].to_vec()))
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

    /// Кроп для NVDEC: остаток обязан быть ровно в пропорции экрана (иначе
    /// картинка на обоях растянется), а числа — чётными (иначе декодер не
    /// поделит цветоразностный канал).
    #[test]
    fn кроп_для_видеокарты_даёт_пропорцию_экрана() {
        let случаи = [
            // (исходник, экран, ожидаемые сверху и слева)
            ((3840, 2160), (2560, 1080), (270, 0)), // обои с котёнком, замер 12.08.2026
            ((3840, 2160), (1920, 1080), (0, 0)),   // те же пропорции — резать нечего
            ((3840, 1080), (2560, 1080), (0, 640)), // исходник шире экрана
            ((1920, 1080), (1080, 1920), (0, 656)), // экран «портретом»
        ];
        for ((sw, sh), (tw, th), ждём) in случаи {
            let (сверху, слева) = cuvid_кроп((sw, sh), (tw, th));
            assert_eq!(
                (сверху, слева), ждём,
                "кроп {sw}×{sh} → {tw}×{th}",
            );
            assert_eq!((сверху % 2, слева % 2), (0, 0), "нечётный кроп {sw}×{sh} → {tw}×{th}");
            // Остаток после кропа — в пропорции экрана (с точностью до
            // округления до чётного).
            let (ow, oh) = (sw - 2 * слева, sh - 2 * сверху);
            let перекос = (ow as f64 / oh as f64) - (tw as f64 / th as f64);
            assert!(
                перекос.abs() < 0.01,
                "после кропа {ow}×{oh} — это не пропорция экрана {tw}×{th}",
            );
        }
    }

    /// Значок загрузки: плашка непрозрачна, точки лежат внутри неё, а фаза
    /// действительно проворачивает кольцо (иначе «крутящийся» значок замер бы
    /// намертво, и заметить это на глаз было бы нечем).
    #[test]
    fn значок_загрузки_рисуется_и_крутится() {
        let кадр = плашка_загрузки(0);
        assert_eq!(кадр.len(), (UI_W * UI_H * 4) as usize);
        let альфа = |px: &[u8], x: u32, y: u32| px[((y * UI_W + x) * 4 + 3) as usize];

        // Углы кадра прозрачны: за пределами плашки не должно быть ничего.
        assert_eq!(альфа(&кадр, 0, 0), 0);
        assert_eq!(альфа(&кадр, UI_W / 2, UI_H / 2), 0);
        // Центр плашки — сама плашка.
        let (cx, cy) = (UI_W - СПИННЕР_ПЛАШКА / 2 - 4, UI_H - СПИННЕР_ПЛАШКА / 2 - 4);
        assert!(альфа(&кадр, cx, cy) > 0, "плашка не нарисована");

        // Точки кольца: между фазами картинка обязана меняться, но пятно
        // остаётся тем же по площади — крутится яркость, а не размер.
        let другой = плашка_загрузки(3);
        assert_ne!(кадр, другой, "фаза не меняет картинку");
        let закрашено = |px: &[u8]| px.chunks_exact(4).filter(|c| c[3] > 0).count();
        assert_eq!(закрашено(&кадр), закрашено(&другой));
    }

    /// Кнопка фильтров: значок рисуется, лежит в своём прямоугольнике и НЕ
    /// совпадает с лупой. Подписей в меню нет — если два значка выйдут
    /// похожими, различить кнопки будет нечем, а на глаз это заметно только в
    /// живом сеансе.
    #[test]
    fn значок_фильтров_отличается_от_лупы() {
        let пусто = || vec![0u8; (UI_W * UI_H * 4) as usize];
        let (fx, fy) = filter_rect();

        let mut воронка = пусто();
        draw_filter_icon(&mut воронка, fx, fy, 0xFFEEEEEE);
        let закрашено = |px: &[u8]| px.chunks_exact(4).filter(|c| c[3] > 0).count();
        assert!(закрашено(&воронка) > 20, "значок не нарисован");

        // Всё нарисованное — внутри кнопки: соседние кнопки задевать нельзя.
        for (i, c) in воронка.chunks_exact(4).enumerate() {
            if c[3] == 0 { continue; }
            let (x, y) = (i as u32 % UI_W, i as u32 / UI_W);
            assert!(
                x >= fx && x < fx + ARROW && y >= fy && y < fy + ARROW,
                "точка ({x},{y}) вне кнопки ({fx},{fy})"
            );
        }

        let mut лупа = пусто();
        draw_search_icon(&mut лупа, fx, fy, 0xFFEEEEEE);
        assert_ne!(воронка, лупа, "воронку не отличить от лупы");

        // И сами кнопки не наезжают друг на друга.
        let (sx, _) = search_rect();
        assert!(fx >= sx + ARROW, "кнопка фильтров налезает на поиск");
    }

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

}

