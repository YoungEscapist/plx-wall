# plx-wall

Обои для [Parallax](https://github.com/YoungEscapist/parallax-wm) — и для любого
другого компоновщика с `wlr-layer-shell`.

Это layer-shell клиент, а не оболочка рабочего стола: он рисует фон на каждом
выходе, носит с собой меню выбора под мышь, крутит живые обои прямо через
`libavcodec` (с аппаратным декодированием в dmabuf там, где видеокарта
позволяет) и считает по картинке палитру, в которую перекрашивается остальная
система.

Parallax ставит его как зависимость — тамошний `install.sh` собирает и кладёт
его сам, — но plx-wall самостоятелен и работает под sway, river, Hyprland и
всем, что понимает `zwlr_layer_shell_v1`.

## Что умеет

- **Обычные и живые обои.** JPEG/PNG/… через `image`, GIF и видео (mp4, webm),
  декодируемое внутри процесса через libavcodec. Видео идёт в dmabuf без единого
  копирования через VAAPI, где драйвер это умеет, и на программный путь, где не
  умеет. На неподвижном экране живые обои спят: кадры не выкладываются, пока их
  никто не просит.
- **Меню выбора.** Обои листаются лентой карточек с глубиной; карточка «далее»
  тянет следующую страницу. Масштаб карточек билинейный, миниатюры лежат в
  `~/.cache/plx-wall/thumbs`.
- **Источники из сети.** wallhaven, Openverse, ежедневный UHD-снимок Bing,
  picsum, Викисклад, библиотека снимков NASA и Чикагский институт искусств; тем
  же меню берутся живые обои. Два последних источника картинок практически
  бездонны (164 714 и 132 740 работ) — именно это и нужно бесконечной ленте,
  у остальных дно близко. Ключи API не нужны нигде.
- **Палитра для всего остального.** Каждая смена обоев пересчитывает 16 цветов
  ANSI плюс фон, текст и акцент, кладёт их в `~/.cache/plx-wall/palette.{json,sh}`
  и `~/.config/ghostty/colors`, красит `fuzzel` и перекрашивает УЖЕ ОТКРЫТЫЕ
  терминалы через OSC 4/10/11/12 — без перезапуска и перечитывания. Тот же
  акцент берёт Parallax для свечения окон. Если есть `~/.local/bin/plx-wall-themes`,
  он запускается для приложений, которые палитру терминала не понимают.

## Установка

```sh
git clone https://github.com/YoungEscapist/plx-wall.git
cd plx-wall
./install.sh
```

Скрипт ставит системные пакеты (Void, Arch, Debian/Ubuntu, Fedora), проверяет
Rust, собирает с `--release` и кладёт бинарь в `~/.local/bin/plx-wall`. Каждая
команда печатается до выполнения, про всё, что делается от root, он спрашивает.
Ключи — `--help`, холостой прогон — `--dry-run`.

Или сразу, без клонирования:

```sh
curl -fsSL https://raw.githubusercontent.com/YoungEscapist/plx-wall/master/install.sh | bash
```

Руками: `cargo build --release && install -Dm755 target/release/plx-wall ~/.local/bin/plx-wall`.

**Зависимости.** pkg-config, C-компилятор, clang (bindgen читает заголовки
ffmpeg ВАШЕЙ системы — см. `build.rs`), клиентские библиотеки wayland и dev-часть
ffmpeg (libavcodec, libavformat, libavutil, libswscale). На запуске нужны ещё
`curl` (источники из сети) и `ffmpeg`/`ffprobe` (миниатюры видео). На NixOS:
`nix-shell`, дальше `cargo build --release`.

## Как пользоваться

```sh
plx-wall                       # запуск, поднимает последние обои
plx-wall ~/Pictures/sky.png    # запуск с этой картинкой, она же запоминается
plx-wall --palette [файл]      # пересчитать палитру и перекрасить терминалы
plx-wall --online картинки код # спросить источники и показать, что нашлось
plx-wall --hwtest видео.mp4    # проверить VAAPI → dmabuf на этой машине
```

Для запуска нужен компоновщик: это layer-shell клиент, и `WAYLAND_DISPLAY`
должен указывать на живой сеанс.

Дальше им правят два сигнала — повесьте их на клавиши в своём компоновщике:

```
pkill -USR1 -x plx-wall     # следующие обои
pkill -USR2 -x plx-wall     # показать/скрыть меню
```

В `config.lua` Parallax (`Super+W` уже занят `toggle_floating`, поэтому Alt):

```lua
bind{ mods = "alt",       key = "w", action = "spawn", cmd = "pkill -USR1 -x plx-wall" }
bind{ mods = "alt+shift", key = "w", action = "spawn", cmd = "pkill -USR2 -x plx-wall" }
```

Состояние живёт в `~/.config/plx-wall`: `wallpapers.json` (список), `current`
(что показано сейчас), `downloaded/` (что принесли источники). Время кадра
показывает `PLX_WALL_PROFILE=1`, частоту живых обоев ограничивает `PLX_WALL_FPS`.

## Лицензия

GPL-3.0-or-later, та же, что у Parallax. См. `LICENSE`.
