//! Вкладка «Configurations»: снимки райсинга композитора.
//!
//! Снимок — копия всего, чем рисуется сеанс: `config.lua` Parallax (там анимации
//! `anim{}`, панель `bar{}`, док `dock{}`, скругления, зазоры, свечение, куб,
//! бинды), его экранные шейдеры, терминал, уведомления, лаунчер и наши
//! собственные выбор обоев и галочка «cycle». Применить снимок — вернуть эти
//! файлы на место, поставить его обои и попросить композитор перечитать
//! конфиг. Перед применением нынешнее состояние кладётся в снимок
//! [`ПЕРЕД_ПРИМЕНЕНИЕМ`]: случайный щелчок по чужому снимку не стирает свой
//! райс безвозвратно.
//!
//! Хранится в `~/.local/share/plx-wall/configs/<id>/`, файлы лежат там под
//! своими путями относительно дома, рядом `name` — подпись в меню.

use std::io;
use std::path::{Path, PathBuf};

/// Файлы райсинга, путями от дома. Нет файла — его нет и в снимке, и при
/// применении он не трогается: снимок не стирает того, чего не знал.
pub const ФАЙЛЫ: &[&str] = &[
    ".config/parallax/config.lua",
    ".config/ghostty/config.ghostty",
    ".config/ghostty/solid",
    ".config/mako/config",
    ".config/rofi/config.rasi",
    ".config/plx-wall/current",
    ".config/plx-wall/cycle",
];

/// Каталоги, которые берутся целиком (только файлы первого уровня).
pub const КАТАЛОГИ: &[&str] = &[".config/parallax/shaders"];

/// Снимок, который сам кладётся перед применением чужого.
pub const ПЕРЕД_ПРИМЕНЕНИЕМ: &str = "before-apply";

#[derive(Clone, Debug, PartialEq)]
pub struct Снимок {
    pub id: String,
    /// Подпись в меню.
    pub имя: String,
    /// Обои снимка — путь из `.config/plx-wall/current`.
    pub обои: Option<String>,
    /// Сколько файлов в снимке.
    pub файлов: usize,
}

pub fn каталог(дом: &Path) -> PathBuf {
    дом.join(".local/share/plx-wall/configs")
}

/// Все снимки: свежие первыми, «до применения» — последним.
pub fn список(дом: &Path) -> Vec<Снимок> {
    let mut все: Vec<Снимок> = std::fs::read_dir(каталог(дом))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| прочитать(дом, &e.file_name().to_string_lossy()))
        .collect();
    все.sort_by(|a, b| {
        (a.id == ПЕРЕД_ПРИМЕНЕНИЕМ)
            .cmp(&(b.id == ПЕРЕД_ПРИМЕНЕНИЕМ))
            .then_with(|| b.id.cmp(&a.id))
    });
    все
}

fn прочитать(дом: &Path, id: &str) -> Option<Снимок> {
    let к = каталог(дом).join(id);
    let имя = std::fs::read_to_string(к.join("name")).ok()?.trim().to_string();
    let обои = std::fs::read_to_string(к.join("files/.config/plx-wall/current"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(Снимок { id: id.to_string(), имя, обои, файлов: файлы_снимка(&к.join("files")).len() })
}

/// Все пути райсинга, что есть в каталоге `корень` (дом или `files/` снимка).
fn файлы_снимка(корень: &Path) -> Vec<String> {
    let mut out: Vec<String> = ФАЙЛЫ
        .iter()
        .filter(|f| корень.join(f).is_file())
        .map(|f| f.to_string())
        .collect();
    for к in КАТАЛОГИ {
        for e in std::fs::read_dir(корень.join(к)).into_iter().flatten().flatten() {
            if e.path().is_file() {
                out.push(format!("{}/{}", к, e.file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

/// Сохранить нынешний райс снимком `id` с подписью `имя` (существующий
/// снимок с тем же id заменяется целиком).
pub fn сохранить_как(дом: &Path, id: &str, имя: &str) -> io::Result<Снимок> {
    let к = каталог(дом).join(id);
    if к.exists() {
        std::fs::remove_dir_all(&к)?;
    }
    let files = к.join("files");
    std::fs::create_dir_all(&files)?;
    for f in файлы_снимка(дом) {
        let куда = files.join(&f);
        if let Some(d) = куда.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::copy(дом.join(&f), &куда)?;
    }
    std::fs::write(к.join("name"), имя)?;
    прочитать(дом, id).ok_or_else(|| io::Error::other("snapshot vanished"))
}

/// Новый снимок с подписью по времени: `18.10 12:40`.
pub fn сохранить(дом: &Path) -> io::Result<Снимок> {
    let (id, имя) = метка_времени();
    сохранить_как(дом, &id, &имя)
}

/// Вернуть файлы снимка на место. Нынешнее состояние уходит в
/// [`ПЕРЕД_ПРИМЕНЕНИЕМ`] (кроме случая, когда применяют его же). Возвращает
/// обои снимка — их ставит вызывающий.
pub fn применить(дом: &Path, id: &str) -> io::Result<Option<String>> {
    let files = каталог(дом).join(id).join("files");
    if !files.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such snapshot"));
    }
    if id != ПЕРЕД_ПРИМЕНЕНИЕМ {
        сохранить_как(дом, ПЕРЕД_ПРИМЕНЕНИЕМ, "before last apply")?;
    }
    for f in файлы_снимка(&files) {
        let куда = дом.join(&f);
        if let Some(d) = куда.parent() {
            std::fs::create_dir_all(d)?;
        }
        // Через временный файл и rename: композитор и терминал читают эти
        // файлы когда хотят, и полузаписанный config.lua уронил бы весь
        // конфиг до встроенного.
        let врем = куда.with_extension("plx-wall-tmp");
        std::fs::copy(files.join(&f), &врем)?;
        std::fs::rename(&врем, &куда)?;
    }
    Ok(прочитать(дом, id).and_then(|с| с.обои))
}

pub fn удалить(дом: &Path, id: &str) -> io::Result<()> {
    // id приходит из нашего же списка, но путь из него собирается — не
    // пускаем наружу каталога снимков.
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad snapshot id"));
    }
    std::fs::remove_dir_all(каталог(дом).join(id))
}

/// (id, подпись): id сортируется по времени, подпись — коротко для меню.
fn метка_времени() -> (String, String) {
    let сек = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Местное время без внешних крейтов: смещение пояса берём у `date`.
    let смещение = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| разобрать_пояс(s.trim()))
        .unwrap_or(0);
    let t = сек + смещение;
    let (г, мес, д) = дата_из_дней(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    let (ч, м, с) = (s / 3600, s / 60 % 60, s % 60);
    (
        format!("{г:04}-{мес:02}-{д:02}_{ч:02}-{м:02}-{с:02}"),
        format!("{д:02}.{мес:02} {ч:02}:{м:02}"),
    )
}

/// `+0300` → секунды.
fn разобрать_пояс(s: &str) -> Option<i64> {
    let (знак, цифры) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if цифры.len() != 4 {
        return None;
    }
    let ч: i64 = цифры[..2].parse().ok()?;
    let м: i64 = цифры[2..].parse().ok()?;
    Some(знак * (ч * 3600 + м * 60))
}

/// Гражданская дата из дней от 1970-01-01 (алгоритм Хиннанта).
fn дата_из_дней(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn дом() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "plx-wall-configs-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".config/parallax/shaders")).unwrap();
        std::fs::create_dir_all(d.join(".config/plx-wall")).unwrap();
        std::fs::write(d.join(".config/parallax/config.lua"), "set{ dissolve = 0 }").unwrap();
        std::fs::write(d.join(".config/parallax/shaders/haze.frag"), "void main(){}").unwrap();
        std::fs::write(d.join(".config/plx-wall/current"), "/w/a.png\n").unwrap();
        d
    }

    #[test]
    fn сохранить_и_применить() {
        let д = дом();
        let с = сохранить_как(&д, "2026-10-18_12-00-00", "18.10 12:00").unwrap();
        assert_eq!(с.файлов, 3);
        assert_eq!(с.обои.as_deref(), Some("/w/a.png"));

        std::fs::write(д.join(".config/parallax/config.lua"), "set{ dissolve = 8 }").unwrap();
        std::fs::remove_file(д.join(".config/parallax/shaders/haze.frag")).unwrap();
        let обои = применить(&д, "2026-10-18_12-00-00").unwrap();
        assert_eq!(обои.as_deref(), Some("/w/a.png"));
        assert_eq!(
            std::fs::read_to_string(д.join(".config/parallax/config.lua")).unwrap(),
            "set{ dissolve = 0 }"
        );
        assert!(д.join(".config/parallax/shaders/haze.frag").is_file());

        // прежнее состояние сохранилось, и оно — в конце списка
        let все = список(&д);
        assert_eq!(все.len(), 2);
        assert_eq!(все[0].id, "2026-10-18_12-00-00");
        assert_eq!(все[1].id, ПЕРЕД_ПРИМЕНЕНИЕМ);
        let до = каталог(&д).join(ПЕРЕД_ПРИМЕНЕНИЕМ).join("files/.config/parallax/config.lua");
        assert_eq!(std::fs::read_to_string(до).unwrap(), "set{ dissolve = 8 }");

        удалить(&д, "2026-10-18_12-00-00").unwrap();
        assert_eq!(список(&д).len(), 1);
        assert!(удалить(&д, "../x").is_err());
        let _ = std::fs::remove_dir_all(&д);
    }

    #[test]
    fn даты() {
        assert_eq!(дата_из_дней(0), (1970, 1, 1));
        assert_eq!(дата_из_дней(20744), (2026, 10, 18));
        assert_eq!(разобрать_пояс("+0300"), Some(10800));
        assert_eq!(разобрать_пояс("-0130"), Some(-5400));
    }
}
