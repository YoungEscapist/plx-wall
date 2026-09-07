#!/usr/bin/env bash
# install.sh — поставить plx-wall: зависимости, Rust, сборка, бинарь в
# ~/.local/bin/plx-wall.
#
#   ./install.sh                    всё по умолчанию
#   ./install.sh --no-deps          не трогать пакеты
#   ./install.sh --update           git pull + пересборка
#   ./install.sh --uninstall        убрать бинарь (конфиг и обои не трогает)
#   ./install.sh --help             все ключи
#
# Работает и из клона, и «с нуля»:
#
#   curl -fsSL https://raw.githubusercontent.com/YoungEscapist/plx-wall/master/install.sh | bash
#
# — во втором случае клонирует дерево в ~/.local/src/plx-wall и перезапускается
# внутри него.
#
# Бинарь кладётся в ~/.local/bin, а не в /usr/bin, по двум причинам: там его
# ищет launch_native.sh у Parallax, и туда можно писать без root — от root тут
# ставятся только системные пакеты, и каждая команда показывается заранее.
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ, и присваивание молча превращается в «command not
# found» (грабля, уже съедавшая chown в сборочных скриптах Parallax).
set -euo pipefail

repo_url=${PLX_WALL_REPO:-https://github.com/YoungEscapist/plx-wall.git}
src_dir_default=${PLX_WALL_SRC:-$HOME/.local/src/plx-wall}
bin_dir=${PLX_WALL_BIN_DIR:-$HOME/.local/bin}

do_deps=1
do_rust=1
do_build=1
do_update=0
do_uninstall=0
assume_yes=0
dry_run=0
jobs=""

usage() {
    cat <<'EOF'
install.sh — установка plx-wall (обои для Parallax и любого layer-shell).

  --no-deps      не ставить системные пакеты
  --no-rust      не проверять и не ставить rustup
  --no-build     не собирать
  --jobs N       передать cargo -j N (на машине с малой памятью)
  --update       git pull в дереве и пересборка
  --uninstall    убрать ~/.local/bin/plx-wall (конфиг и обои остаются)
  -y, --yes      не задавать вопросов
  --dry-run      только показать, что будет сделано
  -h, --help     эта справка

Переменные:
  PLX_WALL_SRC      куда клонировать дерево при запуске через curl
                    (по умолчанию ~/.local/src/plx-wall)
  PLX_WALL_REPO     откуда клонировать
  PLX_WALL_BIN_DIR  куда положить бинарь (по умолчанию ~/.local/bin)
EOF
}

orig_args=("$@")
while [ $# -gt 0 ]; do
    case "$1" in
        --no-deps)   do_deps=0 ;;
        --no-rust)   do_rust=0 ;;
        --no-build)  do_build=0 ;;
        --jobs)      jobs="${2:?--jobs требует число}"; shift ;;
        --jobs=*)    jobs="${1#*=}" ;;
        --update)    do_update=1 ;;
        --uninstall) do_uninstall=1 ;;
        -y|--yes)    assume_yes=1 ;;
        --dry-run)   dry_run=1 ;;
        -h|--help)   usage; exit 0 ;;
        *) echo "Неизвестный ключ: $1 (см. --help)" >&2; exit 2 ;;
    esac
    shift
done

# ── Вывод ─────────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    b=$'\033[1m'; dim=$'\033[2m'; red=$'\033[31m'; yellow=$'\033[33m'; green=$'\033[32m'; off=$'\033[0m'
else
    b=""; dim=""; red=""; yellow=""; green=""; off=""
fi
steps_total=0
step_no=0
step()  { step_no=$((step_no + 1)); printf '\n%s[%d/%d] %s%s\n' "$b" "$step_no" "$steps_total" "$1" "$off"; }
info()  { printf '  %s\n' "$1"; }
note()  { printf '  %s%s%s\n' "$dim" "$1" "$off"; }
warn()  { printf '  %s! %s%s\n' "$yellow" "$1" "$off" >&2; }
die()   { printf '\n%sОшибка: %s%s\n' "$red" "$1" "$off" >&2; exit 1; }
ok()    { printf '  %s✓ %s%s\n' "$green" "$1" "$off"; }

run() {
    printf '  %s$ %s%s\n' "$dim" "$*" "$off"
    [ "$dry_run" = 1 ] && return 0
    "$@"
}

ask() {
    [ "$assume_yes" = 1 ] && return 0
    [ -t 0 ] || return 0
    local answer
    read -r -p "  $1 [Y/n] " answer || return 0
    case "$answer" in [nN]*) return 1 ;; *) return 0 ;; esac
}

sudo_cmd=""
if [ "$(id -u)" != 0 ]; then
    if command -v sudo >/dev/null 2>&1; then sudo_cmd="sudo"
    elif command -v doas >/dev/null 2>&1; then sudo_cmd="doas"
    fi
fi
as_root() {
    if [ "$(id -u)" = 0 ]; then
        run "$@"
    elif [ -n "$sudo_cmd" ]; then
        run "$sudo_cmd" "$@"
    else
        warn "нет ни sudo, ни doas — выполните вручную от root:"
        printf '      %s\n' "$*"
        return 1
    fi
}

# ── Где дерево ────────────────────────────────────────────────────────────────
self=${BASH_SOURCE[0]:-$0}
src=""
if [ -f "$self" ] && [ "$(basename -- "$self")" = install.sh ]; then
    candidate=$(cd -- "$(dirname -- "$(realpath -- "$self")")" && pwd)
    [ -f "$candidate/Cargo.toml" ] && [ -d "$candidate/src" ] && src="$candidate"
fi

if [ -z "$src" ]; then
    command -v git >/dev/null 2>&1 || die "нужен git (или запускайте install.sh из клона)"
    src="$src_dir_default"
    printf '%splx-wall: дерева исходников рядом нет — клонирую.%s\n' "$b" "$off"
    if [ -d "$src/.git" ]; then
        info "уже есть: $src — обновляю"
        run git -C "$src" pull --ff-only
    else
        run mkdir -p "$(dirname -- "$src")"
        run git clone "$repo_url" "$src"
    fi
    [ "$dry_run" = 1 ] && { info "дальше запустился бы $src/install.sh"; exit 0; }
    exec bash "$src/install.sh" ${orig_args[@]+"${orig_args[@]}"}
fi

cd "$src"

# ── Снятие ────────────────────────────────────────────────────────────────────
if [ "$do_uninstall" = 1 ]; then
    steps_total=1
    step "Убираю бинарь"
    run rm -f "$bin_dir/plx-wall"
    printf '\n%sГотово.%s Список обоев (~/.config/plx-wall) и скачанные картинки остались.\n' "$b" "$off"
    exit 0
fi

[ "$do_update" = 1 ] && steps_total=$((steps_total + 1))
[ "$do_deps" = 1 ]   && steps_total=$((steps_total + 1))
[ "$do_rust" = 1 ]   && steps_total=$((steps_total + 1))
[ "$do_build" = 1 ]  && steps_total=$((steps_total + 2))   # сборка и установка бинаря

printf '%splx-wall — установка из исходников%s\n' "$b" "$off"
note "дерево: $src"
note "бинарь: $bin_dir/plx-wall"
[ "$dry_run" = 1 ] && printf '  %s(--dry-run: ничего выполнено не будет)%s\n' "$yellow" "$off"

# ── Обновление дерева ─────────────────────────────────────────────────────────
if [ "$do_update" = 1 ]; then
    step "Обновляю дерево"
    if [ -d .git ]; then
        run git pull --ff-only || warn "git pull не прошёл — собираю то, что есть"
    else
        warn "это не git-клон: обновлять нечего"
    fi
fi

# ── Зависимости ───────────────────────────────────────────────────────────────
#
# Список короткий и весь на виду — отдельного install-deps.sh, как у Parallax,
# тут не нужно. Главное в нём — ffmpeg: привязки к нему генерирует bindgen ПО
# ЗАГОЛОВКАМ ЭТОЙ СИСТЕМЫ (см. build.rs), поэтому нужны и dev-часть ffmpeg, и
# clang. Без clang сборка падает на «Unable to find libclang», без заголовков —
# на «не найден libavcodec» ещё в build.rs.
#
# Библиотек wayland в списке НЕТ намеренно: `wayland-client` говорит по
# протоколу сам, своим бэкендом на Rust, и в готовом бинаре libwayland-client
# не значится вовсе (проверено ldd). Ставить -dev пакет ради него значило бы
# просить у человека то, чем никто не пользуется.
if [ "$do_deps" = 1 ]; then
    step "Системные зависимости"
    deps_cmd=()
    if command -v xbps-install >/dev/null 2>&1; then
        deps_manager="Void (xbps)"
        # ffmpeg в Void переезжал между именами: ffmpeg6-devel есть не везде.
        ffmpeg_pkg=ffmpeg-devel
        # Именно `grep -q .`: xbps-query -Rs отвечает нулём и на пустую выдачу,
        # то есть «нашлось» без проверки вывода означало бы «нашлось всегда».
        xbps-query -Rs ffmpeg6-devel 2>/dev/null | grep -q . && ffmpeg_pkg=ffmpeg6-devel
        deps_cmd=(xbps-install -y base-devel pkg-config clang "$ffmpeg_pkg" curl)
    elif command -v pacman >/dev/null 2>&1; then
        deps_manager="Arch (pacman)"
        deps_cmd=(pacman -S --needed --noconfirm base-devel pkgconf clang ffmpeg curl)
    elif command -v apt >/dev/null 2>&1; then
        deps_manager="Debian/Ubuntu (apt)"
        deps_cmd=(apt install -y build-essential pkg-config libclang-dev
            libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
            ffmpeg curl)
    elif command -v dnf >/dev/null 2>&1; then
        deps_manager="Fedora (dnf)"
        # ffmpeg-devel живёт в rpmfusion; в основном репозитории есть урезанный
        # ffmpeg-free-devel — его хватает, декодеры у живых обоев обычные.
        ffmpeg_pkg=ffmpeg-free-devel
        dnf list --quiet ffmpeg-devel >/dev/null 2>&1 && ffmpeg_pkg=ffmpeg-devel
        deps_cmd=(dnf install -y gcc pkgconf-pkg-config clang-devel "$ffmpeg_pkg" curl)
    fi
    if [ ${#deps_cmd[@]} -eq 0 ]; then
        warn "пакетный менеджер не опознан (не Void, Arch, Debian/Ubuntu или Fedora)."
        warn "Нужны: pkg-config, компилятор C, clang и dev-пакеты ffmpeg"
        warn "(libavcodec, libavformat, libavutil, libswscale). На NixOS: nix-shell."
        ask "Продолжить без установки пакетов?" || exit 1
    else
        info "# $deps_manager"
        printf '  %s\n' "${deps_cmd[*]}"
        if ask "Поставить эти пакеты?"; then
            as_root "${deps_cmd[@]}" || die "не удалось поставить пакеты"
            ok "пакеты на месте"
        else
            note "пропущено — сборка упадёт, если чего-то не хватает"
        fi
    fi
fi

# ── Rust ──────────────────────────────────────────────────────────────────────
if [ "$do_rust" = 1 ]; then
    step "Rust"
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    if command -v cargo >/dev/null 2>&1; then
        ok "cargo есть, rustc $(rustc --version 2>/dev/null | awk '{print $2}')"
    else
        warn "cargo не найден"
        if ask "Поставить Rust через rustup (rustup.rs, ставится в ~/.rustup и ~/.cargo)?"; then
            command -v curl >/dev/null 2>&1 || die "нужен curl, чтобы скачать rustup"
            if [ "$dry_run" = 1 ]; then
                note '$ curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
            else
                curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
                PATH="$HOME/.cargo/bin:$PATH"
            fi
            command -v cargo >/dev/null 2>&1 || [ "$dry_run" = 1 ] || die "rustup поставился, а cargo не нашёлся"
        else
            die "без cargo собрать нечего"
        fi
    fi
fi

# ── Сборка ────────────────────────────────────────────────────────────────────
if [ "$do_build" = 1 ]; then
    step "Сборка"
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 || die "cargo не найден (--no-rust пропустил проверку?)"

    # Линкер подбираем, а не зашиваем: mold есть далеко не у всех, и зашитый в
    # .cargo/config.toml он ронял чужой клон на «cannot find -fuse-ld=mold» —
    # ровно та грабля, из-за которой у Parallax этот файл опустел.
    build_flags=""
    if command -v mold >/dev/null 2>&1; then
        build_flags="-C link-arg=-fuse-ld=mold"; note "линкер: mold"
    elif command -v ld.lld >/dev/null 2>&1; then
        build_flags="-C link-arg=-fuse-ld=lld"; note "линкер: lld"
    else
        note "линкер: системный ld"
    fi
    export RUSTFLAGS="$build_flags"

    cargo_args=()
    [ -n "$jobs" ] && cargo_args+=(-j "$jobs")
    build_started=$(date +%s)
    run cargo build --release ${cargo_args[@]+"${cargo_args[@]}"} \
        || die "сборка не прошла (чаще всего не хватает dev-пакетов ffmpeg или clang)"
    [ "$dry_run" = 1 ] || ok "собрано за $(( $(date +%s) - build_started )) с"

    step "Бинарь в $bin_dir"
    run mkdir -p "$bin_dir"
    run install -m755 target/release/plx-wall "$bin_dir/plx-wall"
    ok "$bin_dir/plx-wall"
    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) warn "$bin_dir не в PATH — добавьте его, иначе команда plx-wall не найдётся"
           note "(Parallax зовёт бинарь по полному пути, ему PATH не нужен)" ;;
    esac
fi

printf '\n%sГотово.%s\n' "$green$b" "$off"
cat <<EOF

  Запуск (нужен живой сеанс wayland):
      plx-wall                     поднять обои
      plx-wall ~/Pictures/x.png    показать и запомнить эту картинку
      plx-wall --palette           пересчитать палитру и перекрасить терминалы

  Дальше им правят два сигнала — повесьте их на клавиши:
      pkill -USR1 -x plx-wall      следующие обои
      pkill -USR2 -x plx-wall      показать/скрыть меню выбора

  В сеансе Parallax обои поднимает launch_native.sh сам, руками ничего не надо.
  Обновиться потом: cd $src && ./install.sh --update

EOF
