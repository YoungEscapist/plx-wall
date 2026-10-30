//! Применение снимка райсинга без отката биндов (30.10.2026).
//!
//! Снимок хранит `config.lua` целиком, и применить старый снимок значило
//! вернуть и старые бинды: 26.10 так пропали лаунчер okno на Super+S и
//! правило окна Waydroid, добавленные после снимка. Теперь из снимка берётся
//! вид — всё, кроме вызовов из [`СВОИ`], — а бинды и правила окон остаются
//! нынешними.
//!
//! Как: оба файла режутся на операторы верхнего уровня (`bind{…}`, цикл
//! `for … end`, `set{…}`) вместе с комментариями вплотную над ними и под ними
//! и пустыми строками перед ними. Основа — снимок: его бинды, которых нет в
//! нынешнем файле дословно, выбрасываются; нынешние бинды, которых нет в
//! снимке, встают следом за тем, что стояло перед ними в нынешнем файле, —
//! если такой сосед в основе нашёлся, иначе там, где остановился обход.
//! Lua не разобрался (незакрытая скобка или строка) — `None`, и снимок
//! кладётся целиком, как раньше.
//!
//! Чего не умеет: бинд, который ссылается на `local`, заведённую в нынешнем
//! файле и отсутствующую в снимке, останется без неё (в конфигах parallax
//! таких `local` нет).

use std::collections::HashMap;

/// Вызовы, которые остаются нынешними: клавиши, аккорды мыши, жесты
/// сенсорного экрана, правила окон. Оператор считается своим, если все
/// вызовы в нём вне скобок — отсюда (цикл `for i = 1, 9 do bind{…} end` тоже).
pub const СВОИ: &[&str] = &["bind", "mouse", "touch", "window"];

/// Слить снимок с нынешним файлом: вид — из снимка, бинды — нынешние.
pub fn слить(снимок: &str, нынешний: &str) -> Option<String> {
    let с = разрезать(снимок)?;
    let н = разрезать(нынешний)?;

    // Основа — снимок без тех своих операторов, которых больше нет.
    let mut есть = счёт(н.iter().filter(|к| к.свой));
    let основа: Vec<&Кусок> = с.iter().filter(|к| !к.свой || взять(&mut есть, к)).collect();

    // Нынешние свои, которых в основе нет, — новые (уже стоящие в основе в
    // другом порядке не дублируются).
    let mut в_основе = счёт(основа.iter().copied().filter(|к| к.свой));
    let новый: Vec<bool> = н.iter().map(|к| к.свой && !взять(&mut в_основе, к)).collect();

    // Что из нынешнего файла дословно стоит в основе — по порядку.
    let mut пара: Vec<Option<usize>> = vec![None; н.len()];
    let mut занят = vec![false; основа.len()];
    let mut курсор = 0;
    for (j, к) in н.iter().enumerate() {
        if к.суть.is_empty() || новый[j] {
            continue;
        }
        if let Some(i) = (курсор..основа.len()).find(|&i| !занят[i] && основа[i].суть == к.суть) {
            занят[i] = true;
            пара[j] = Some(i);
            курсор = i + 1;
        }
    }

    // Новый встаёт между совпавшими соседями по нынешнему файлу; внутри
    // промежутка — после стольких операторов основы, сколько перед ним в
    // нынешнем файле изменившихся (вид поменялся — стоят они там же).
    let mut вставки: Vec<(usize, &Кусок)> = Vec::new();
    for (j, к) in н.iter().enumerate() {
        if !новый[j] {
            continue;
        }
        let (от, после) = match (0..j).rev().find_map(|k| пара[k].map(|i| (k, i))) {
            Some((k, i)) => (i + 1, k + 1),
            None => (0, 0),
        };
        let до = (j + 1..н.len()).find_map(|k| пара[k]).unwrap_or(основа.len()).max(от);
        let изменённых = (после..j)
            .filter(|&k| пара[k].is_none() && !новый[k] && !н[k].суть.is_empty())
            .count();
        вставки.push(((от + изменённых).min(до), к));
    }
    вставки.sort_by_key(|&(место, _)| место);

    let mut out = String::new();
    let mut в = вставки.iter().peekable();
    for (i, к) in основа.iter().enumerate() {
        while let Some((_, н)) = в.next_if(|(место, _)| *место == i) {
            out.push_str(&н.текст);
        }
        out.push_str(&к.текст);
    }
    for (_, н) in в {
        out.push_str(&н.текст);
    }
    if !снимок.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

fn счёт<'a>(куски: impl Iterator<Item = &'a Кусок>) -> HashMap<&'a str, usize> {
    let mut есть = HashMap::new();
    for к in куски {
        *есть.entry(к.суть.as_str()).or_default() += 1;
    }
    есть
}

/// Есть ли ещё такой же кусок в счёте — и вычесть его.
fn взять(есть: &mut HashMap<&str, usize>, к: &Кусок) -> bool {
    match есть.get_mut(к.суть.as_str()) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// Оператор верхнего уровня (или отдельный комментарий, или хвост из
/// пустых строк) вместе с пустыми строками перед ним.
#[derive(Debug)]
struct Кусок {
    /// Как есть, с пустыми строками перед ним — так он и вставляется.
    текст: String,
    /// Без пустых строк и хвостовых пробелов — по ней сравниваем.
    суть: String,
    свой: bool,
}

/// Что известно о строке файла после разбора.
#[derive(Clone, Copy, Default)]
struct Строка {
    пустая: bool,
    /// Кода нет — только комментарий (или середина длинного).
    комментарий: bool,
    /// Скобки и блоки (`function`/`do`/`if`/`repeat` … `end`/`until`) на конце строки.
    глубина: i32,
    /// Конец строки внутри длинной строки или комментария.
    длинное: bool,
    /// Строка кончается оператором — выражение продолжается дальше.
    продолжение: bool,
}

/// Имя, за которым сразу идёт `{`, `(` или строка, — вызов.
struct Вызов {
    строка: usize,
    имя: String,
    /// Глубина скобок у имени: 0 — вызов на уровне оператора.
    скобки: i32,
}

fn разрезать(текст: &str) -> Option<Vec<Кусок>> {
    let mut текст = текст.to_string();
    if !текст.ends_with('\n') {
        текст.push('\n');
    }
    let (строки, вызовы) = разобрать(&текст)?;
    let линии: Vec<&str> = текст.split_inclusive('\n').collect();
    debug_assert_eq!(линии.len(), строки.len());
    let n = строки.len();

    let собрать = |a: usize, b: usize| линии[a..b].concat();
    let суть = |a: usize, b: usize| {
        линии[a..b].iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
    };

    let mut куски = Vec::new();
    let mut i = 0;
    while i < n {
        let начало = i;
        while i < n && строки[i].пустая {
            i += 1;
        }
        if i == n {
            куски.push(Кусок { текст: собрать(начало, n), суть: String::new(), свой: false });
            break;
        }
        let начало_сути = i;
        while i < n && строки[i].комментарий {
            i += 1;
        }
        // Комментарий, за которым пустая строка, — сам по себе.
        if i > начало_сути && (i == n || строки[i].пустая) {
            куски.push(Кусок { текст: собрать(начало, i), суть: суть(начало_сути, i), свой: false });
            continue;
        }
        let начало_кода = i;
        loop {
            let конец = строки[i].глубина == 0 && !строки[i].длинное && !строки[i].продолжение;
            i += 1;
            if конец || i == n {
                break;
            }
        }
        // Комментарий вплотную под оператором и до пустой строки — его.
        let mut j = i;
        while j < n && строки[j].комментарий {
            j += 1;
        }
        if j > i && (j == n || строки[j].пустая) {
            i = j;
        }
        let имена: Vec<&str> = вызовы
            .iter()
            .filter(|в| в.строка >= начало_кода && в.строка < i && в.скобки == 0)
            .map(|в| в.имя.as_str())
            .collect();
        let свой = !имена.is_empty() && имена.iter().all(|и| СВОИ.contains(и));
        куски.push(Кусок { текст: собрать(начало, i), суть: суть(начало_сути, i), свой });
    }
    Some(куски)
}

#[derive(Clone, Copy, PartialEq)]
enum Где {
    Код,
    Комментарий,
    ДлинныйКомментарий(usize),
    Строка(u8),
    ДлиннаяСтрока(usize),
}

/// `[` + `=`×n + `[` с позиции `i`: уровень n.
fn длинная_скобка(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'=') {
        j += 1;
    }
    (b.get(j) == Some(&b'[')).then_some(j - i - 1)
}

/// `]` + `=`×n + `]` с позиции `i`.
fn закрывает(b: &[u8], i: usize, n: usize) -> bool {
    b.get(i) == Some(&b']')
        && (1..=n).all(|k| b.get(i + k) == Some(&b'='))
        && b.get(i + n + 1) == Some(&b']')
}

/// Разбор Lua до строк: где код, где комментарий, какая глубина на концах
/// строк и какие вызовы где. `None` — файл не замкнут.
fn разобрать(текст: &str) -> Option<(Vec<Строка>, Vec<Вызов>)> {
    let b = текст.as_bytes();
    let mut строки = Vec::new();
    let mut вызовы = Vec::new();
    let (mut скобки, mut блоки) = (0i32, 0i32);
    let mut где = Где::Код;
    let mut строка = 0usize;
    let (mut код, mut коммент, mut продолжение) = (false, false, false);
    // Последнее имя — вдруг за ним откроется вызов.
    let mut имя: Option<(String, i32)> = None;
    // Последним в короткой строке был `\` — перевод строки экранирован.
    let mut экран = false;

    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            if let Где::Строка(_) = где {
                // Короткая строка переносится только через `\` в конце.
                if !экран {
                    return None;
                }
            }
            if где == Где::Комментарий {
                где = Где::Код;
            }
            let длинное = matches!(где, Где::ДлинныйКомментарий(_) | Где::ДлиннаяСтрока(_) | Где::Строка(_));
            экран = false;
            строки.push(Строка {
                пустая: !код && !коммент && !длинное,
                комментарий: !код && коммент,
                глубина: скобки + блоки,
                длинное,
                продолжение,
            });
            строка += 1;
            // Строка, начатая внутри длинного, — продолжение того же.
            код = matches!(где, Где::ДлиннаяСтрока(_));
            коммент = matches!(где, Где::ДлинныйКомментарий(_));
            i += 1;
            continue;
        }
        match где {
            Где::Комментарий => i += 1,
            Где::ДлинныйКомментарий(n) => {
                if закрывает(b, i, n) {
                    где = Где::Код;
                    i += n + 2;
                } else {
                    i += 1;
                }
            }
            Где::ДлиннаяСтрока(n) => {
                if закрывает(b, i, n) {
                    где = Где::Код;
                    i += n + 2;
                } else {
                    i += 1;
                }
            }
            Где::Строка(q) => {
                if c == b'\\' {
                    // Перед переводом строки — строка идёт дальше: сам \n
                    // обработает начало цикла.
                    if b.get(i + 1) == Some(&b'\n') {
                        экран = true;
                        i += 1;
                    } else {
                        i += 2;
                    }
                } else {
                    if c == q {
                        где = Где::Код;
                    }
                    i += 1;
                }
            }
            Где::Код => {
                if c == b' ' || c == b'\t' || c == b'\r' {
                    i += 1;
                    continue;
                }
                if c == b'-' && b.get(i + 1) == Some(&b'-') {
                    коммент = true;
                    match длинная_скобка(b, i + 2) {
                        Some(n) => {
                            где = Где::ДлинныйКомментарий(n);
                            i += 2 + n + 2;
                        }
                        None => {
                            где = Где::Комментарий;
                            i += 2;
                        }
                    }
                    continue;
                }
                код = true;
                let вызов = |имя: &mut Option<(String, i32)>, вызовы: &mut Vec<Вызов>| {
                    if let Some((и, с)) = имя.take() {
                        вызовы.push(Вызов { строка, имя: и, скобки: с });
                    }
                };
                if c == b'"' || c == b'\'' {
                    вызов(&mut имя, &mut вызовы);
                    где = Где::Строка(c);
                    продолжение = false;
                    i += 1;
                    continue;
                }
                if let Some(n) = длинная_скобка(b, i) {
                    вызов(&mut имя, &mut вызовы);
                    где = Где::ДлиннаяСтрока(n);
                    продолжение = false;
                    i += n + 2;
                    continue;
                }
                if c.is_ascii_alphabetic() || c == b'_' {
                    let s = i;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    let слово = &текст[s..i];
                    имя = None;
                    продолжение = matches!(слово, "and" | "or" | "not");
                    match слово {
                        "function" | "do" | "if" | "repeat" => блоки += 1,
                        "end" | "until" => {
                            блоки -= 1;
                            if блоки < 0 {
                                return None;
                            }
                        }
                        "then" | "else" | "elseif" | "for" | "while" | "in" | "local"
                        | "return" | "and" | "or" | "not" | "nil" | "true" | "false"
                        | "break" | "goto" => {}
                        _ => имя = Some((слово.to_string(), скобки)),
                    }
                    continue;
                }
                if c.is_ascii_digit() {
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                        i += 1;
                    }
                    имя = None;
                    продолжение = false;
                    continue;
                }
                match c {
                    b'(' | b'{' | b'[' => {
                        if c != b'[' {
                            вызов(&mut имя, &mut вызовы);
                        }
                        скобки += 1;
                    }
                    b')' | b'}' | b']' => {
                        скобки -= 1;
                        if скобки < 0 {
                            return None;
                        }
                    }
                    _ => {}
                }
                имя = None;
                продолжение = matches!(
                    c,
                    b'=' | b',' | b'.' | b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>' | b'~'
                        | b':' | b'#' | b'&' | b'|'
                );
                i += 1;
            }
        }
    }
    // Последняя строка без перевода (сюда не попадаем: разрезать добавляет \n).
    if где != Где::Код && где != Где::Комментарий || скобки != 0 || блоки != 0 {
        return None;
    }
    Some((строки, вызовы))
}

#[cfg(test)]
mod tests {
    use super::*;

    const СНИМОК: &str = "\
-- Parallax
set{ gaps = 8 }

-- ── Лаунчер ──
-- rofi через plx-launcher.
bind{ mods = \"super\", key = \"s\", action = \"spawn\", cmd = \"plx-launcher\" }

bind{ mods = \"super\", key = \"Print\", action = \"screenshot\" }
-- Super+C: история буфера в rofi.
bind{ mods = \"super\", key = \"c\", action = \"spawn\",
      cmd = \"pkill -x rofi || cliphist list | rofi -dmenu\" }

for i = 1, 9 do
  bind{ mods = \"super\", key = tostring(i), action = \"view_tag\", tag = i }
end

anim{ window_open = 300 }
";

    const НЫНЕШНИЙ: &str = "\
-- Parallax
set{ gaps = 12 }

-- ── Лаунчер ──
-- okno — лаунчер обычным окном.
bind{ mods = \"super\", key = \"s\", action = \"spawn\", cmd = \"okno\" }
window{ app_id = \"okno\", float = true, above = true }

-- Waydroid — портретным окном.
window{ app_id = \"waydroid.*\", float = true, w = 540, h = 960 }
-- Прежний — rofi: cmd = \"plx-launcher\".

bind{ mods = \"super\", key = \"Print\", action = \"screenshot\" }
-- Super+C: история буфера в okno.
bind{ mods = \"super\", key = \"c\", action = \"spawn\",
      cmd = \"pkill -x okno || cliphist list | okno -dmenu\" }

for i = 1, 9 do
  bind{ mods = \"super\", key = tostring(i), action = \"view_tag\", tag = i }
end

set{ rounding = 0 }
anim{ window_open = 360 }
";

    #[test]
    fn вид_из_снимка_бинды_нынешние() {
        let итог = слить(СНИМОК, НЫНЕШНИЙ).unwrap();
        assert_eq!(
            итог,
            "\
-- Parallax
set{ gaps = 8 }

-- ── Лаунчер ──
-- okno — лаунчер обычным окном.
bind{ mods = \"super\", key = \"s\", action = \"spawn\", cmd = \"okno\" }
window{ app_id = \"okno\", float = true, above = true }

-- Waydroid — портретным окном.
window{ app_id = \"waydroid.*\", float = true, w = 540, h = 960 }
-- Прежний — rofi: cmd = \"plx-launcher\".

bind{ mods = \"super\", key = \"Print\", action = \"screenshot\" }
-- Super+C: история буфера в okno.
bind{ mods = \"super\", key = \"c\", action = \"spawn\",
      cmd = \"pkill -x okno || cliphist list | okno -dmenu\" }

for i = 1, 9 do
  bind{ mods = \"super\", key = tostring(i), action = \"view_tag\", tag = i }
end

anim{ window_open = 300 }
"
        );
    }

    #[test]
    fn сам_с_собой_без_перемен() {
        for т in [СНИМОК, НЫНЕШНИЙ, "set{ a = 1 }", "", "\n\n-- только комментарий\n"] {
            assert_eq!(слить(т, т).as_deref(), Some(т));
        }
    }

    #[test]
    fn удалённый_бинд_не_возвращается() {
        let снимок = "bind{ key = \"a\" }\nbind{ key = \"b\" }\nset{ x = 1 }\n";
        let нынешний = "bind{ key = \"a\" }\nset{ x = 2 }\n";
        assert_eq!(слить(снимок, нынешний).unwrap(), "bind{ key = \"a\" }\nset{ x = 1 }\n");
    }

    #[test]
    fn смешанный_блок_и_вызовы_в_скобках() {
        // set внутри цикла — это вид, весь цикл из снимка; tostring внутри
        // скобок бинда не делает его чужим.
        let снимок = "for i = 1, 2 do\n  set{ x = i }\n  bind{ key = tostring(i) }\nend\n";
        let нынешний = "for i = 1, 3 do\n  set{ x = i }\n  bind{ key = tostring(i) }\nend\nbind{ key = \"z\" }\n";
        assert_eq!(слить(снимок, нынешний).unwrap(), format!("{снимок}bind{{ key = \"z\" }}\n"));
    }

    #[test]
    fn строки_и_длинные_комментарии() {
        let нынешний = "--[[ старое:\nbind{ key = \"x\" }\n]]\nbind{ key = \"}\" } -- скобка в строке\nlocal s = [==[\n{ ]] ]==]\nwindow{ app_id = 'a\\'b' }\n";
        let снимок = "set{ y = 1 }\n";
        let итог = слить(снимок, нынешний).unwrap();
        // Длинный комментарий вплотную над биндом — его; `local` с длинной
        // строкой — вид, его в снимке нет, а место его занимает set снимка:
        // бинд был выше него, window — ниже.
        assert_eq!(
            итог,
            "--[[ старое:\nbind{ key = \"x\" }\n]]\nbind{ key = \"}\" } -- скобка в строке\nset{ y = 1 }\nwindow{ app_id = 'a\\'b' }\n"
        );
    }

    #[test]
    fn незамкнутый_файл_не_трогаем() {
        assert!(слить("set{ x = 1\n", "bind{}\n").is_none());
        assert!(слить("set{}\n", "bind{ key = \"x }\n").is_none());
        assert!(слить("end\n", "").is_none());
    }

    #[test]
    fn продолжение_выражения() {
        // Оператор в конце строки и `\` в короткой строке тянут оператор дальше.
        let к = разрезать("local cmd = \"a\" ..\n  \"b\"\nbind{ cmd = \"x\\\ny\" }\nset{}\n").unwrap();
        assert_eq!(к.len(), 3);
        assert!(!к[0].свой && к[1].свой && !к[2].свой);
    }
}
