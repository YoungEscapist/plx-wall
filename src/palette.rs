//! Палитра из обоев.
//!
//! Идея всей затеи: цвет живёт в ОДНОМ месте — в 16 цветах ANSI-палитры
//! терминала. Обои меняются → dwall пересчитывает палитру и рассылает её
//! escape-последовательностями во все живые pty. Всё, что рисует себя
//! обычными ANSI-цветами (cmatrix, fastfetch, starship, tuimus), меняет вид
//! само, без перезапуска и без единой строчки в их конфигах.
//!
//! Что получается на выходе:
//! - `~/.cache/dwall/palette.json` — для тех, кто хочет цвета явно (tuimus);
//! - `~/.cache/dwall/palette.sh`   — то же для шелла;
//! - `~/.config/ghostty/colors`    — новые окна ghostty стартуют уже в теме;
//! - OSC 4/10/11/12 во все `/dev/pts/*` — УЖЕ ОТКРЫТЫЕ окна перекрашиваются.
//!
//! Почему цвета не берутся из картинки «как есть»: тогда красный в терминале
//! перестал бы быть красным, и вывод `git diff` или `ls` читался бы наугад.
//! Поэтому оттенки ANSI сдвигаются к тону обоев лишь частично (см. СДВИГ), а
//! насыщенность и светлота выравниваются — цвета остаются узнаваемыми, но
//! вся палитра явно принадлежит одной картинке.

use std::io::Write;

/// Насколько тон каждого ANSI-цвета тянется к тону обоев. 1.0 — полностью
/// (красный станет цветом обоев), 0.0 — палитра не меняется вовсе.
const СДВИГ: f32 = 0.28;

/// Потолок сдвига в градусах.
///
/// Без него доля от РАССТОЯНИЯ бьёт по дальним цветам сильнее всего: на тёплых
/// обоях (тон 30°) синий 220° уезжал на 268° и переставал быть синим — вывод
/// становился фиолетово-розовым месивом. Замер 06.08.2026 на обоях с котёнком:
/// было #7f59af, стало #5964af. Потолок держит дальние тона узнаваемыми, а
/// близкие к обоям всё равно притягиваются полностью.
const ПОТОЛОК: f32 = 22.0;

pub struct Palette {
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub cursor: [u8; 3],
    pub accent: [u8; 3],
    pub sel_bg: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

// ── Цветовые преобразования ─────────────────────────────────────────────────

fn rgb2hsl(c: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    ((h + 360.0) % 360.0, s, l)
}

fn hsl2rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        (((r + m) * 255.0).clamp(0.0, 255.0)) as u8,
        (((g + m) * 255.0).clamp(0.0, 255.0)) as u8,
        (((b + m) * 255.0).clamp(0.0, 255.0)) as u8,
    ]
}

/// Кратчайший поворот от `from` к `to` по цветовому кругу, взятый долей `k`.
fn сдвинуть_тон(from: f32, to: f32, k: f32) -> f32 {
    let mut d = to - from;
    if d > 180.0 { d -= 360.0; }
    if d < -180.0 { d += 360.0; }
    let сдвиг = (d * k).clamp(-ПОТОЛОК, ПОТОЛОК);
    (from + сдвиг + 360.0) % 360.0
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

// ── Разбор картинки ─────────────────────────────────────────────────────────

/// Главный тон картинки и её средняя насыщенность.
///
/// Считается по гистограмме из 36 корзин по тону, вес пикселя — его
/// насыщенность: серый фон (небо, асфальт) почти ничего не вносит, а мелкое
/// яркое пятно заметно. Совсем тёмное и совсем белое выбрасываем — у них тон
/// шумный и определяется случайными единицами младших битов.
fn главный_тон(px: &[u8]) -> (f32, f32, f32) {
    const КОРЗИН: usize = 36;
    let mut вес = [0f32; КОРЗИН];
    let mut сумма_s = 0f32;
    let mut сумма_l = 0f32;
    let mut учтено = 0f32;

    for c in px.chunks_exact(4) {
        let (h, s, l) = rgb2hsl([c[0], c[1], c[2]]);
        if l < 0.06 || l > 0.95 { continue; }
        учтено += 1.0;
        сумма_s += s;
        сумма_l += l;
        if s < 0.12 { continue; }
        let i = ((h / 360.0 * КОРЗИН as f32) as usize).min(КОРЗИН - 1);
        вес[i] += s * s;
    }

    if учтено < 1.0 {
        return (150.0, 0.35, 0.45);
    }
    let средняя_s = (сумма_s / учтено).clamp(0.15, 0.75);
    let средняя_l = (сумма_l / учтено).clamp(0.1, 0.9);

    // Берём корзину вместе с соседями: тон редко попадает ровно в центр, а
    // без сглаживания две почти равные соседние корзины дают дёрганый выбор —
    // близкие обои давали бы заметно разный тон.
    let mut лучшая = 0usize;
    let mut лучший_вес = -1f32;
    for i in 0..КОРЗИН {
        let сумма = вес[(i + КОРЗИН - 1) % КОРЗИН] + вес[i] + вес[(i + 1) % КОРЗИН];
        if сумма > лучший_вес {
            лучший_вес = сумма;
            лучшая = i;
        }
    }
    // Пустая гистограмма — картинка серая. Тон брать неоткуда, оставляем
    // зелёный Void: лучше знакомый цвет, чем случайный.
    if лучший_вес <= 0.0 {
        return (150.0, 0.25, средняя_l);
    }
    let тон = (лучшая as f32 + 0.5) / КОРЗИН as f32 * 360.0;
    (тон, средняя_s, средняя_l)
}

impl Palette {
    pub fn from_pixels(px: &[u8]) -> Palette {
        let (тон, s, _l) = главный_тон(px);

        // Фон — тёмный, но не чёрный: в нём должен читаться тон обоев,
        // иначе смысл всей затеи теряется. Насыщенность держим низкой, чтобы
        // текст поверх не резал глаз.
        let bg = hsl2rgb(тон, (s * 0.55).clamp(0.10, 0.30), 0.075);
        let fg = hsl2rgb(тон, (s * 0.25).clamp(0.05, 0.18), 0.86);
        let accent = hsl2rgb(тон, s.max(0.45), 0.58);
        let cursor = hsl2rgb(тон, s.max(0.40), 0.66);
        let sel_bg = hsl2rgb(тон, s.max(0.35), 0.32);

        // Опорные тона ANSI. Каждый тянется к тону обоев на СДВИГ, светлота и
        // насыщенность общие — отсюда ощущение одного набора.
        const ОПОРЫ: [f32; 6] = [
            0.0,   // красный
            120.0, // зелёный
            48.0,  // жёлтый
            220.0, // синий
            300.0, // пурпурный
            180.0, // голубой
        ];
        let база_s = s.clamp(0.35, 0.70);
        let mut ansi = [[0u8; 3]; 16];

        // 0 — «чёрный». Не чистый ноль: cmatrix и компания красят им фон, и на
        // угольном тоне обоев это выглядит куском темноты, а не дырой.
        ansi[0] = hsl2rgb(тон, (s * 0.5).clamp(0.08, 0.25), 0.13);
        ansi[7] = hsl2rgb(тон, (s * 0.2).clamp(0.04, 0.15), 0.78);
        ansi[8] = hsl2rgb(тон, (s * 0.4).clamp(0.06, 0.22), 0.32);
        ansi[15] = hsl2rgb(тон, (s * 0.12).clamp(0.02, 0.10), 0.95);

        for (i, опора) in ОПОРЫ.iter().enumerate() {
            let h = сдвинуть_тон(*опора, тон, СДВИГ);
            // 1..6 — обычные, 9..14 — яркие.
            let место = match i {
                0 => 1, 1 => 2, 2 => 3, 3 => 4, 4 => 5, _ => 6,
            };
            ansi[место] = hsl2rgb(h, база_s, 0.52);
            ansi[место + 8] = hsl2rgb(h, (база_s * 1.1).min(0.85), 0.68);
        }

        Palette { bg, fg, cursor, accent, sel_bg, ansi }
    }
}

// ── Запись и рассылка ───────────────────────────────────────────────────────

fn кэш() -> String {
    format!("{}/.cache/dwall", crate::home())
}

impl Palette {
    fn json(&self) -> String {
        let список = self
            .ansi
            .iter()
            .map(|c| format!("\"{}\"", hex(*c)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{{\n  \"bg\": \"{}\",\n  \"fg\": \"{}\",\n  \"cursor\": \"{}\",\n  \
             \"accent\": \"{}\",\n  \"sel_bg\": \"{}\",\n  \"ansi\": [{}]\n}}\n",
            hex(self.bg), hex(self.fg), hex(self.cursor),
            hex(self.accent), hex(self.sel_bg), список
        )
    }

    fn shell(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("DWALL_BG={}\n", hex(self.bg)));
        s.push_str(&format!("DWALL_FG={}\n", hex(self.fg)));
        s.push_str(&format!("DWALL_ACCENT={}\n", hex(self.accent)));
        for (i, c) in self.ansi.iter().enumerate() {
            s.push_str(&format!("DWALL_ANSI{i}={}\n", hex(*c)));
        }
        s
    }

    /// Фрагмент конфига ghostty. Подключается из основного через
    /// `config-file = ?colors` — новые окна открываются уже в теме обоев.
    fn ghostty(&self) -> String {
        let mut s = String::from("# Создаётся dwall при смене обоев. Править нет смысла.\n");
        s.push_str(&format!("background = {}\n", hex(self.bg)));
        s.push_str(&format!("foreground = {}\n", hex(self.fg)));
        s.push_str(&format!("cursor-color = {}\n", hex(self.cursor)));
        s.push_str(&format!("selection-background = {}\n", hex(self.sel_bg)));
        s.push_str(&format!("selection-foreground = {}\n", hex(self.ansi[15])));
        for (i, c) in self.ansi.iter().enumerate() {
            s.push_str(&format!("palette = {i}={}\n", hex(*c)));
        }
        s
    }

    /// Те же цвета, но escape-последовательностями — для уже открытых окон.
    fn osc(&self) -> String {
        let mut s = String::new();
        for (i, c) in self.ansi.iter().enumerate() {
            s.push_str(&format!("\x1b]4;{i};{}\x1b\\", hex(*c)));
        }
        s.push_str(&format!("\x1b]10;{}\x1b\\", hex(self.fg)));
        s.push_str(&format!("\x1b]11;{}\x1b\\", hex(self.bg)));
        s.push_str(&format!("\x1b]12;{}\x1b\\", hex(self.cursor)));
        s
    }
}

/// Разослать палитру во все псевдотерминалы пользователя.
///
/// Запись в `/dev/pts/N` уходит в мастер-конец, то есть в сам эмулятор, и он
/// разбирает её как обычный вывод — поэтому перекрашивается и терминал, и всё,
/// что в нём сейчас нарисовано (ncurses перерисует экран следующим кадром).
///
/// Открываем ТОЛЬКО с O_NONBLOCK: если у pty никто не читает мастер (зависший
/// эмулятор, отцепленный сеанс), обычный open+write повис бы намертво и утащил
/// за собой поток обоев.
fn разослать(osc: &str) {
    let Ok(dir) = std::fs::read_dir("/dev/pts") else { return };
    let uid = unsafe { libc::getuid() };
    for e in dir.flatten() {
        let path = e.path();
        // ptmx — мастер-мультиплексор, не терминал; запись в него бессмысленна.
        if path.file_name().map(|n| n == "ptmx").unwrap_or(false) { continue; }
        let Ok(md) = e.metadata() else { continue };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid { continue; }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        else { continue };
        let _ = f.write_all(osc.as_bytes());
    }
}

use std::os::unix::fs::OpenOptionsExt;

/// Посчитать палитру по обоям и разложить её по местам. Зовётся из фонового
/// потока: декодирование картинки тут стоит столько же, сколько сами обои.
///
/// Возвращает посчитанную палитру — ей пользуется режим `dwall --palette`,
/// чтобы показать результат, не поднимая ни композитора, ни обоев.
pub fn apply(path: &str) -> Option<Palette> {
    // Картинку берём уменьшенной: для гистограммы хватает и 64×64, а полный
    // 4K-кадр считался бы десятки миллисекунд впустую.
    let (w, h, px) = crate::load_image(path)?;
    if w == 0 || h == 0 { return None; }
    let мелкая = crate::resize_ex(w, h, &px, 64, 64, true);

    let p = Palette::from_pixels(&мелкая);

    let dir = кэш();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/palette.json"), p.json());
    let _ = std::fs::write(format!("{dir}/palette.sh"), p.shell());

    let g = format!("{}/.config/ghostty", crate::home());
    if std::path::Path::new(&g).is_dir() {
        let _ = std::fs::write(format!("{g}/colors"), p.ghostty());
    }

    разослать(&p.osc());
    обновить_темы();
    Some(p)
}

/// Перерисовать темы приложений, которые не умеют палитру терминала
/// (Vesktop, Waterfox, AyuGram) — этим занимается отдельный сценарий
/// `~/.local/bin/dwall-themes`.
///
/// Запускаем и НЕ ЖДЁМ: внутри там ffmpeg (кадр обоев для фона Telegram), это
/// секунды. Ждать их в потоке палитры незачем — обои от этого не зависят.
/// Нет сценария — молча идём дальше: dwall обязан работать и без него.
fn обновить_темы() {
    let путь = format!("{}/.local/bin/dwall-themes", crate::home());
    if !std::path::Path::new(&путь).is_file() {
        return;
    }
    // Сироту не оставляем: без wait процесс-зомби висел бы до конца сессии,
    // а обои меняются часто.
    if let Ok(mut дитя) = std::process::Command::new(&путь)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = дитя.wait();
        });
    }
}

/// `dwall --palette [файл]` — пересчитать палитру и показать её.
///
/// Без файла берутся обои, показанные сейчас. Нужен и для проверки (палитра
/// считается без композитора), и по-человечески: перекрасить терминалы, не
/// трогая обои.
pub fn режим_палитры(файл: Option<String>) -> i32 {
    let Some(path) = файл.or_else(crate::load_current) else {
        eprintln!("dwall --palette: не задан файл и нет запомненных обоев");
        return 1;
    };
    let Some(p) = apply(&path) else {
        eprintln!("dwall --palette: не удалось разобрать {path}");
        return 1;
    };
    println!("обои:  {path}");
    println!("фон:   {}", hex(p.bg));
    println!("текст: {}", hex(p.fg));
    println!("акцент:{}", hex(p.accent));
    print!("ANSI:  ");
    for (i, c) in p.ansi.iter().enumerate() {
        // Каждый цвет показываем им же — видно сразу, что получилось.
        print!("\x1b[48;2;{};{};{}m  \x1b[0m", c[0], c[1], c[2]);
        if i == 7 { print!(" "); }
    }
    println!();
    println!("записано: {}/palette.json, palette.sh, ~/.config/ghostty/colors", кэш());
    0
}
