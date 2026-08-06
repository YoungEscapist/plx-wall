//! Обои из сети.
//!
//! Два источника, по одному на каждый род обоев:
//! - **wallhaven.cc** — статичные картинки. У него есть настоящий открытый
//!   API без ключа, поэтому здесь всё честно и стабильно.
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
    /// Статичные картинки, wallhaven.cc.
    Картинки,
    /// Живые обои, moewalls.com.
    Живые,
}

/// Одна находка в выдаче.
#[derive(Clone, Debug)]
pub struct Находка {
    /// Адрес миниатюры — её и показываем в сетке.
    pub миниатюра: String,
    /// Что скачивать. У wallhaven это сразу файл, у moewalls — страница
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
        Источник::Картинки => wallhaven(страница, ширина, высота, запрос),
        Источник::Живые => moewalls(страница, запрос),
    }
}

fn wallhaven(страница: u32, ширина: u32, высота: u32, запрос: &str) -> Result<Vec<Находка>, String> {
    // purity=100 — только безопасное; categories=100 — только «general»
    // (без аниме и людей: обои для рабочего стола, а не галерея).
    // atleast — не показывать то, что мельче монитора: растянутая картинка
    // выглядит мылом, а понять это по миниатюре нельзя.
    // При поиске сортировка меняется на «по совпадению»: с toplist запрос
    // почти не влияет на выдачу — сверху всё равно всплывает вечная классика.
    let сортировка = if запрос.is_empty() { "toplist" } else { "relevance" };
    let url = format!(
        "https://wallhaven.cc/api/v1/search?categories=100&purity=100&sorting={сортировка}\
         &order=desc&atleast={ширина}x{высота}&page={страница}&q={}",
        закодировать(запрос)
    );
    let тело = curl_текст(&url).ok_or("wallhaven не ответил")?;
    let json: serde_json::Value =
        serde_json::from_str(&тело).map_err(|e| format!("wallhaven: непонятный ответ ({e})"))?;

    let список = json
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("wallhaven: в ответе нет списка")?;

    Ok(список
        .iter()
        .filter_map(|item| {
            let миниатюра = item.pointer("/thumbs/small")?.as_str()?.to_string();
            let цель = item.get("path")?.as_str()?.to_string();
            let имя = item.get("id")?.as_str().unwrap_or("wallhaven").to_string();
            Some(Находка { миниатюра, цель, источник: Источник::Картинки, имя })
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
