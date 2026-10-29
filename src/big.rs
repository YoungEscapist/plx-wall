//! Big picture: обои — огромная картинка в своём НАСТОЯЩЕМ размере, как
//! gigapixel-обои у driftwm. Композитор кладёт её на холст, она едет и
//! зумится вместе с ним и повторяется с периодом в свой размер — как cycle, но
//! без единого ужатия.
//!
//! **Почему не одним буфером, как cycle.** Картинка 43200×21600 — это 3.7 ГБ
//! RGBA: ни в общую память по два слота, ни в одну текстуру (у видеокарт
//! предел 16–32 тысячи точек по стороне) она не лезет, а на отдалении одна
//! текстура без мипмапов рябит.
//!
//! **Как.** Картинка ОДИН раз режется на пирамиду плиток 512×512 в
//! `~/.cache/plx-wall/big/<ключ>/`: уровень 0 — настоящий размер, каждый
//! следующий вдвое меньше, пока картинка не станет не больше 2048 по стороне.
//! Этот последний уровень — ОБЗОР: его plx-wall кладёт на фоновый слой, как
//! обычные обои, а композитору говорит `wallpaper big <каталог>`. parallax
//! растягивает обзор до настоящего размера подложкой и поверх кладёт видимые
//! плитки нужного зуму уровня (его src/большие_обои.rs читает этот каталог:
//! `plx-big.txt`, `<уровень>/<x>_<y>.png`).

use std::path::{Path, PathBuf};

use image::ImageEncoder;
use rayon::prelude::*;

/// Сторона плитки.
pub const ПЛИТКА: u32 = 512;
/// Обзор — не больше этого по стороне. 2048×1024 — 8 МБ на слот буфера, а
/// точка обзора на экране у parallax видна только там, где она не крупнее
/// точки экрана: ближе он берёт плитки.
pub const ОБЗОР: u32 = 2048;
/// Описание пирамиды. Пишется последним: есть файл — есть и все плитки.
const ОПИСАНИЕ: &str = "plx-big.txt";
const ФАЙЛ_ОБЗОРА: &str = "overview.png";
/// Сколько нарезанных картинок держать на диске. Пирамида гигапиксельной
/// картинки — пара гигабайт, копить их незачем.
const ХРАНИТЬ: usize = 3;

pub struct Готово {
    pub каталог: PathBuf,
    /// Обзор в RGBA — то, что ляжет на фоновый слой.
    pub обзор: (u32, u32, Vec<u8>),
}

pub fn каталог_кэша() -> PathBuf {
    PathBuf::from(format!("{}/.cache/plx-wall/big", crate::home()))
}

/// Пирамида для картинки: готовая с диска или нарезанная сейчас. Долго (на
/// гигапиксельной картинке — десятки секунд), звать из фонового потока.
/// `None` — картинка не читается или на диске нет места; тогда обои ставятся
/// как в cycle, одним буфером.
pub fn подготовить(path: &str) -> Option<Готово> {
    // Две нарезки разом писали бы в один и тот же каталог `.part`.
    static ОДНА: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _одна = ОДНА.lock().unwrap_or_else(|e| e.into_inner());

    let ключ = crate::ключ_миниатюры(path);
    let каталог = каталог_кэша().join(ключ.trim_end_matches(".rgba"));
    if let Some(обзор) = прочитать_обзор(&каталог) {
        отметить(&каталог);
        return Some(Готово { каталог, обзор });
    }

    let начало = std::time::Instant::now();
    let часть = каталог.with_extension("part");
    let _ = std::fs::remove_dir_all(&часть);
    let итог = нарезать(path, &часть);
    if итог.is_none() {
        let _ = std::fs::remove_dir_all(&часть);
        eprintln!("plx-wall: big picture: не вышло нарезать {path}");
        return None;
    }
    let _ = std::fs::remove_dir_all(&каталог);
    if std::fs::rename(&часть, &каталог).is_err() {
        let _ = std::fs::remove_dir_all(&часть);
        return None;
    }
    убрать_старые(&каталог);
    let обзор = прочитать_обзор(&каталог)?;
    eprintln!(
        "plx-wall: big picture: {path} нарезан за {:.1} с → {}",
        начало.elapsed().as_secs_f32(),
        каталог.display(),
    );
    Some(Готово { каталог, обзор })
}

fn нарезать(path: &str, часть: &Path) -> Option<()> {
    std::fs::create_dir_all(часть).ok()?;
    // Предел памяти у `image` по умолчанию — 512 МБ, а картинка тут как раз
    // крупнее. RGB, а не RGBA: обои непрозрачны, и это на четверть меньше
    // памяти на самом большом уровне.
    let mut чтение = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    чтение.no_limits();
    let картинка = чтение.decode().ok()?.into_rgb8();
    let (w0, h0) = картинка.dimensions();
    if w0 == 0 || h0 == 0 {
        return None;
    }
    let (mut w, mut h, mut точки) = (w0, h0, картинка.into_raw());
    let mut уровни = 0u32;
    while w.max(h) > ОБЗОР {
        записать_уровень(&часть.join(уровни.to_string()), w, h, &точки)?;
        (w, h, точки) = вдвое(w, h, &точки);
        уровни += 1;
    }
    сохранить_png(&часть.join(ФАЙЛ_ОБЗОРА), w, h, &точки)?;
    std::fs::write(
        часть.join(ОПИСАНИЕ),
        format!("plx-wall big 1\nsize {w0} {h0}\ntile {ПЛИТКА}\nlevels {уровни}\n"),
    )
    .ok()
}

/// Свой пул: нарезка грузит все потоки на секунды, а общий пул rayon режет
/// кадр меню — оно бы встало. Половина ядер и пониженный приоритет.
fn пул() -> &'static rayon::ThreadPool {
    static ПУЛ: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    ПУЛ.get_or_init(|| {
        let ядер = std::thread::available_parallelism().map_or(2, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads((ядер / 2).max(2))
            .thread_name(|i| format!("plx-wall-big-{i}"))
            .start_handler(|_| crate::понизить_приоритет_потока())
            .build()
            .expect("пул big picture")
    })
}

fn записать_уровень(папка: &Path, w: u32, h: u32, точки: &[u8]) -> Option<()> {
    std::fs::create_dir_all(папка).ok()?;
    let (nx, ny) = (w.div_ceil(ПЛИТКА), h.div_ceil(ПЛИТКА));
    let (w, п) = (w as usize, ПЛИТКА as usize);
    let все = пул().install(|| {
        (0..nx * ny).into_par_iter().all(|i| {
            let (tx, ty) = ((i % nx) as usize, (i / nx) as usize);
            let (x0, y0) = (tx * п, ty * п);
            let tw = (w - x0).min(п);
            let th = (h as usize - y0).min(п);
            let mut плитка = Vec::with_capacity(tw * th * 3);
            for y in y0..y0 + th {
                let s = (y * w + x0) * 3;
                плитка.extend_from_slice(&точки[s..s + tw * 3]);
            }
            сохранить_png(&папка.join(format!("{tx}_{ty}.png")), tw as u32, th as u32, &плитка).is_some()
        })
    });
    все.then_some(())
}

/// Следующий уровень: среднее квадрата 2×2, размер с округлением вверх
/// (последний столбец и строка нечётной стороны берут сами себя). Так же
/// считает размеры уровней parallax.
fn вдвое(w: u32, h: u32, точки: &[u8]) -> (u32, u32, Vec<u8>) {
    let (нw, нh) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
    let (w, h) = (w as usize, h as usize);
    let mut итог = vec![0u8; нw * нh * 3];
    пул().install(|| {
        итог.par_chunks_mut(нw * 3).enumerate().for_each(|(y, строка)| {
            let (верх, низ) = (2 * y * w, (2 * y + 1).min(h - 1) * w);
            for x in 0..нw {
                let (л, п) = (2 * x, (2 * x + 1).min(w - 1));
                for c in 0..3 {
                    let сумма = точки[(верх + л) * 3 + c] as u32
                        + точки[(верх + п) * 3 + c] as u32
                        + точки[(низ + л) * 3 + c] as u32
                        + точки[(низ + п) * 3 + c] as u32;
                    строка[x * 3 + c] = ((сумма + 2) / 4) as u8;
                }
            }
        });
    });
    (нw as u32, нh as u32, итог)
}

/// PNG без потерь, но с быстрым сжатием: плиток тысячи, и медленное сжатие
/// растянуло бы нарезку в разы ради десятка процентов места.
fn сохранить_png(путь: &Path, w: u32, h: u32, rgb: &[u8]) -> Option<()> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let файл = std::fs::File::create(путь).ok()?;
    PngEncoder::new_with_quality(std::io::BufWriter::new(файл), CompressionType::Fast, FilterType::Adaptive)
        .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
        .ok()
}

fn прочитать_обзор(каталог: &Path) -> Option<(u32, u32, Vec<u8>)> {
    if !каталог.join(ОПИСАНИЕ).is_file() {
        return None;
    }
    let обзор = image::open(каталог.join(ФАЙЛ_ОБЗОРА)).ok()?.into_rgba8();
    let (w, h) = обзор.dimensions();
    Some((w, h, обзор.into_raw()))
}

/// Время правки описания — когда пирамидой пользовались последний раз; по
/// нему `убрать_старые` решает, что выбросить.
fn отметить(каталог: &Path) {
    if let Ok(ф) = std::fs::File::options().append(true).open(каталог.join(ОПИСАНИЕ)) {
        let _ = ф.set_modified(std::time::SystemTime::now());
    }
}

fn убрать_старые(новый: &Path) {
    let Ok(список) = std::fs::read_dir(каталог_кэша()) else { return };
    let mut пирамиды: Vec<(std::time::SystemTime, PathBuf)> = список
        .flatten()
        .map(|з| з.path())
        .filter(|п| п.as_path() != новый)
        .filter_map(|п| {
            let когда = std::fs::metadata(п.join(ОПИСАНИЕ)).and_then(|м| м.modified()).ok()?;
            Some((когда, п))
        })
        .collect();
    пирамиды.sort_by(|раньше, позже| позже.0.cmp(&раньше.0));
    for (_, п) in пирамиды.into_iter().skip(ХРАНИТЬ - 1) {
        let _ = std::fs::remove_dir_all(п);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn вдвое_с_нечётной_стороной() {
        // 3×1: (10, 30) → 20, последняя точка берёт сама себя.
        let точки = [10, 10, 10, 30, 30, 30, 90, 90, 90];
        let (w, h, итог) = вдвое(3, 1, &точки);
        assert_eq!((w, h), (2, 1));
        assert_eq!(итог, vec![20, 20, 20, 90, 90, 90]);
    }

    #[test]
    fn пирамида_режется_и_читается() {
        let корень = std::env::temp_dir().join(format!("plx-wall-big-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&корень);
        std::fs::create_dir_all(&корень).unwrap();
        // 5000×1200: уровни 5000 и 2500 режутся, обзор — 1250×300.
        let исходник = корень.join("big.png");
        let картинка = image::RgbImage::from_fn(5000, 1200, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
        картинка.save(&исходник).unwrap();
        let часть = корень.join("pyr");
        нарезать(исходник.to_str().unwrap(), &часть).unwrap();
        let описание = std::fs::read_to_string(часть.join(ОПИСАНИЕ)).unwrap();
        assert_eq!(описание, "plx-wall big 1\nsize 5000 1200\ntile 512\nlevels 2\n");
        // Уровень 0: 10×3 плиток, последняя по x узкая (5000 = 9×512 + 392).
        let угол = image::open(часть.join("0/9_2.png")).unwrap();
        assert_eq!((угол.width(), угол.height()), (392, 1200 - 2 * 512));
        assert!(часть.join("1/4_1.png").is_file());
        assert!(!часть.join("2").exists());
        let (w, h, rgba) = прочитать_обзор(&часть).unwrap();
        assert_eq!((w, h, rgba.len()), (1250, 300, 1250 * 300 * 4));
        let _ = std::fs::remove_dir_all(&корень);
    }
}
