//! Обои из сети.
//!
//! Два источника, по одному на каждый род обоев:
//! - **openverse.org** — статичные картинки. Открытый JSON API без ключа.
//!   Стоит здесь вместо wallhaven.cc, который недоступен из России.
//!   Openverse — фотобанк CC-картинок, а не сборник обоев, поэтому выдачу
//!   приходится сужать фильтрами (см. [`openverse`]), а миниатюры брать не у
//!   него (см. [`превью`]).
//! - **moewalls.com** — живые обои. Открытого API у сайта нет, но он на
//!   WordPress, и его `/wp-json/wp/v2/posts` отдаёт список постов вместе с
//!   адресом картинки-превью (в поле `yoast_head_json.og_image`). Сам ролик
//!   ищем по странице поста — уже когда обои ВЫБРАЛИ, одним запросом.
//!
//! Почему адрес ролика не собирается по шаблону, хотя он предсказуемый
//! (`/wp-content/uploads/preview/<год>/<slug>-preview.webm`): год в адресе —
//! это год ЗАГРУЗКИ файла, а он не всегда совпадает с датой поста, и на
//! старых обоях шаблон промахивается. Один лишний запрос при скачивании
//! дешевле, чем «выбрал — не открылось».
//!
//! Ходим наружу через `curl`, а не через HTTP-клиент на Rust: в dwall нет ни
//! асинхронного рантайма, ни TLS-обвязки, и тащить их ради двух запросов —
//! это плюс полторы сотни зависимостей. Тем же способом (через процесс)
//! работает и декодер обоев, так что приём для этого кода не новый.
//!
//! ВАЖНО: всё здесь БЛОКИРУЮЩЕЕ и обязано вызываться из отдельного потока —
//! главный цикл не должен ждать сеть ни секунды.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Представляемся браузером: moewalls отвечает на «непонятные» подписи
/// страницей-заглушкой, а не данными.
const БРАУЗЕР: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Сколько ждать сеть. Больше нет смысла: при таком ожидании человек уже
/// закрыл меню.
const ЖДАТЬ: &str = "20";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Источник {
    /// Статичные картинки, openverse.org.
    Картинки,
    /// Живые обои, moewalls.com.
    Живые,
}

/// Одна находка в выдаче.
#[derive(Clone, Debug)]
pub struct Находка {
    /// Адрес миниатюры — её и показываем в сетке.
    pub миниатюра: String,
    /// Что скачивать. У openverse это сразу файл, у moewalls — страница
    /// поста, на которой ещё надо найти ролик (см. [`скачать`]).
    pub цель: String,
    pub источник: Источник,
    /// Из чего строить имя файла.
    pub имя: String,
}

fn curl(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-sL", "--max-time", ЖДАТЬ, "-A", БРАУЗЕР, url])
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn curl_текст(url: &str) -> Option<String> {
    String::from_utf8(curl(url)?).ok()
}

/// Проценты вместо всего, что не буква и не цифра.
///
/// Свой, а не крейт: нужно ровно это, а запрос приходит от человека и может
/// содержать что угодно — пробелы, кириллицу, амперсанд. Без кодирования
/// амперсанд разрезал бы запрос на два параметра, а пробел вовсе сломал бы
/// адрес.
fn закодировать(текст: &str) -> String {
    let mut out = String::with_capacity(текст.len());
    for b in текст.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Список обоев со страницы `страница` (нумерация с единицы).
///
/// `запрос` пустой — показываем то, что источник считает лучшим.
pub fn список(
    источник: Источник,
    страница: u32,
    ширина: u32,
    высота: u32,
    запрос: &str,
) -> Result<Vec<Находка>, String> {
    match источник {
        Источник::Картинки => openverse(страница, ширина, высота, запрос),
        Источник::Живые => moewalls(страница, запрос),
    }
}

/// Адрес уменьшенной картинки для сетки.
///
/// Собственные миниатюры Openverse (`/v1/images/<id>/thumb/`) НЕ используем по
/// двум причинам сразу. Первая: они попросту не работают — на проверке
/// 09.08.2026 все до одной ответили 424 «Thumbnail unavailable from provider».
/// Вторая, более принципиальная: анонимный доступ к API ограничен 20 запросами
/// в минуту и 200 в сутки, а одна страница сетки — это 20 миниатюр, то есть
/// минутный лимит выбирался бы целиком на первой же странице.
///
/// Поэтому уменьшенный вариант просим прямо у хранилища провайдера, и к самому
/// Openverse остаётся ОДИН запрос на страницу — за списком.
fn превью(файл: &str) -> String {
    // Wikimedia (весь верх выдачи с нашими фильтрами — из неё). Произвольную
    // ширину через /thumb/ она больше не отдаёт: 400 и «Use thumbnail sizes
    // listed on ...». Документированный Special:FilePath с width работает и
    // отдаёт ~100 КБ вместо нескольких мегабайт оригинала.
    if файл.starts_with("https://upload.wikimedia.org/") {
        if let Some(имя) = файл.rsplit('/').next().filter(|s| !s.is_empty()) {
            return format!("https://commons.wikimedia.org/wiki/Special:FilePath/{имя}?width=640");
        }
    }
    // Flickr держит размер прямо в имени файла: _b — 1024 px, _n — 320 px.
    if файл.contains("staticflickr.com") {
        if let Some(без) = файл.strip_suffix("_b.jpg") {
            return format!("{без}_n.jpg");
        }
    }
    // Прочие провайдеры: сойдёт и оригинал — ужимать его всё равно нам.
    файл.to_string()
}

fn openverse(страница: u32, ширина: u32, высота: u32, запрос: &str) -> Result<Vec<Находка>, String> {
    // page_size=20 — потолок для анонима: с 21 API отвечает 401, а не режет
    // выдачу. Пустой запрос допустим, отдельная «сортировка по умолчанию»,
    // как у wallhaven, здесь не нужна.
    //
    // size=large и aspect_ratio=wide обязательны: Openverse — фотобанк, и без
    // них в выдачу лезут вертикальные снимки и мелочь, которую на широкий
    // экран не растянуть. С ними ширина находок начинается от ~1400 px.
    let url = format!(
        "https://api.openverse.org/v1/images/?page_size=20&size=large&aspect_ratio=wide\
         &page={страница}&q={}",
        закодировать(запрос)
    );
    let тело = curl_текст(&url).ok_or("openverse не ответил")?;
    let json: serde_json::Value =
        serde_json::from_str(&тело).map_err(|e| format!("openverse: непонятный ответ ({e})"))?;

    let список = json
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or("openverse: в ответе нет списка")?;

    Ok(список
        .iter()
        .filter_map(|item| {
            let цель = item.get("url")?.as_str()?.to_string();

            // Аналог wallhaven-овского atleast, но мягче: там фильтровал сам
            // сервер и запас картинок был бездонный, здесь же строгое «не
            // мельче монитора» на 4K оставило бы от страницы пару находок.
            // Отсекаем только явную мелочь — вдвое меньше экрана; остальное
            // dwall кадрирует сам.
            let w = item.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
            let h = item.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
            if w * 2 < ширина as u64 || h * 2 < высота as u64 {
                return None;
            }

            let имя = item.get("id")?.as_str()?.to_string();
            Some(Находка { миниатюра: превью(&цель), цель, источник: Источник::Картинки, имя })
        })
        .collect())
}

fn moewalls(страница: u32, запрос: &str) -> Result<Vec<Находка>, String> {
    let url = format!(
        "https://moewalls.com/wp-json/wp/v2/posts?per_page=24&page={страница}&search={}",
        закодировать(запрос)
    );
    let тело = curl_текст(&url).ok_or("moewalls не ответил")?;
    let json: serde_json::Value =
        serde_json::from_str(&тело).map_err(|e| format!("moewalls: непонятный ответ ({e})"))?;

    let список = json.as_array().ok_or("moewalls: в ответе нет списка")?;

    Ok(список
        .iter()
        .filter_map(|item| {
            let миниатюра = item
                .pointer("/yoast_head_json/og_image/0/url")?
                .as_str()?
                .to_string();
            let цель = item.get("link")?.as_str()?.to_string();
            let имя = item.get("slug").and_then(|v| v.as_str()).unwrap_or("moewalls").to_string();
            Some(Находка { миниатюра, цель, источник: Источник::Живые, имя })
        })
        .collect())
}

/// Скачать миниатюру. Возвращает сырые байты файла — разбирать их будет
/// вызывающий (у него уже есть декодер).
pub fn миниатюра(находка: &Находка) -> Option<Vec<u8>> {
    curl(&находка.миниатюра)
}

/// Найти на странице поста moewalls адрес ролика.
///
/// Ищем `src="...preview....webm"` — так размечен проигрыватель на странице.
/// Адрес там относительный, поэтому приделываем схему и хост.
fn ролик_со_страницы(html: &str) -> Option<String> {
    let ключ = "src=\"";
    let mut хвост = html;
    while let Some(i) = хвост.find(ключ) {
        хвост = &хвост[i + ключ.len()..];
        let Some(конец) = хвост.find('"') else { break };
        let адрес = &хвост[..конец];
        if адрес.contains("preview") && (адрес.ends_with(".webm") || адрес.ends_with(".mp4")) {
            return Some(if адрес.starts_with("http") {
                адрес.to_string()
            } else if let Some(хвост) = адрес.strip_prefix('/') {
                format!("https://moewalls.com/{хвост}")
            } else {
                format!("https://moewalls.com/{адрес}")
            });
        }
    }
    None
}

/// Безопасное имя файла: у обоев в названии бывает что угодно.
fn безопасное(имя: &str) -> String {
    имя.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .take(60)
        .collect()
}

/// Скачивает выбранные обои в `каталог` и отдаёт путь к файлу.
pub fn скачать(находка: &Находка, каталог: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(каталог).map_err(|e| format!("не создать каталог: {e}"))?;

    let файл = match находка.источник {
        Источник::Картинки => находка.цель.clone(),
        Источник::Живые => {
            let html = curl_текст(&находка.цель).ok_or("страница обоев не открылась")?;
            ролик_со_страницы(&html).ok_or("на странице не нашлось ролика")?
        }
    };

    let расширение = файл
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_lowercase();

    let путь = каталог.join(format!("{}.{расширение}", безопасное(&находка.имя)));

    // Уже качали — второй раз незачем.
    if std::fs::metadata(&путь).map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(путь);
    }

    let данные = curl(&файл).ok_or("файл не скачался")?;
    if данные.is_empty() {
        return Err("сервер отдал пустой файл".into());
    }
    std::fs::write(&путь, &данные).map_err(|e| format!("не записать файл: {e}"))?;

    Ok(путь)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ролик_находится_по_относительному_адресу() {
        let html = r#"<video><source src="/wp-content/uploads/preview/2026/x-preview.webm"></video>"#;
        assert_eq!(
            ролик_со_страницы(html).as_deref(),
            Some("https://moewalls.com/wp-content/uploads/preview/2026/x-preview.webm")
        );
    }

    #[test]
    fn посторонние_src_пропускаются() {
        // На странице полно картинок и скриптов — брать надо только ролик.
        let html = r#"<img src="/logo.png"><script src="/a.js"></script>
                      <video src="https://moewalls.com/wp-content/uploads/preview/2026/y-preview.mp4">"#;
        assert_eq!(
            ролик_со_страницы(html).as_deref(),
            Some("https://moewalls.com/wp-content/uploads/preview/2026/y-preview.mp4")
        );
    }

    /// Wikimedia отдаёт превью только через Special:FilePath: произвольная
    /// ширина в /thumb/ отвечает 400. Оригинал там — единицы мегабайт, и
    /// страница сетки из двадцати таких загрузила бы под сотню мегабайт.
    #[test]
    fn превью_викимедиа_идёт_через_filepath() {
        assert_eq!(
            превью("https://upload.wikimedia.org/wikipedia/commons/e/e1/Mekong.jpg"),
            "https://commons.wikimedia.org/wiki/Special:FilePath/Mekong.jpg?width=640"
        );
    }

    /// У flickr размер зашит в имя файла: _b — 1024 px, _n — 320 px.
    #[test]
    fn превью_flickr_уменьшается_суффиксом() {
        assert_eq!(
            превью("https://live.staticflickr.com/213/491726079_4f46636859_b.jpg"),
            "https://live.staticflickr.com/213/491726079_4f46636859_n.jpg"
        );
    }

    /// Незнакомый провайдер — берём оригинал, ужмём сами. Молча отдать пустую
    /// строку было бы хуже: сетка показала бы дыру вместо картинки.
    #[test]
    fn превью_незнакомого_провайдера_остаётся_оригиналом() {
        let адрес = "https://example.org/img/a.jpg";
        assert_eq!(превью(адрес), адрес);
    }

    /// Запрос пишет человек, и в нём бывает что угодно. Амперсанд без
    /// кодирования разрезал бы адрес на два параметра, пробел сломал бы его
    /// целиком, а кириллица ушла бы в сеть как попало.
    #[test]
    fn запрос_кодируется() {
        assert_eq!(закодировать("mountain"), "mountain");
        assert_eq!(закодировать("night city"), "night+city");
        assert_eq!(закодировать("a&b=c"), "a%26b%3Dc");
        assert_eq!(закодировать("кот"), "%D0%BA%D0%BE%D1%82");
        assert_eq!(закодировать(""), "");
    }

    #[test]
    fn без_ролика_возвращается_ничего() {
        assert_eq!(ролик_со_страницы("<img src=\"/logo.png\">"), None);
    }

    /// Имя приходит с чужого сайта, а по нему создаётся ФАЙЛ. Проверяем не
    /// побуквенное совпадение, а то, ради чего функция написана: из имени не
    /// должно остаться ничего, чем можно уйти из своего каталога.
    #[test]
    fn имя_файла_обеззараживается() {
        let опасное = безопасное("аниме/../../etc/passwd");
        assert!(!опасное.contains('/'), "срезы пути убраны: {опасное}");
        assert!(!опасное.contains('.'), "точки убраны: {опасное}");
        assert!(опасное.ends_with("etc_passwd"), "остальное узнаваемо: {опасное}");

        // Обычное имя не портим.
        assert_eq!(безопасное("itachi-uchiha-2026"), "itachi-uchiha-2026");

        // И длину ограничиваем — имена у обоев бывают безразмерные.
        assert!(безопасное(&"я".repeat(200)).len() <= 60);
    }
}
