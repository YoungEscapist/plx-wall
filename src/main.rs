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
const UI_H: u32 = 480;
const CORNER: u32 = 14;
const THUMB: u32 = 155;
const COLS: u32 = 4;
const GAP: u32 = 12;
const BG_COLOR: u32 = 0xCC111111; // ARGB
const CARD_COLOR: u32 = 0xFF222222;
const PLUS_COLOR: u32 = 0xFF333333;
const HOVER_COLOR: u32 = 0xFF444444;
const SEL_COLOR: u32 = 0xFFFFFFFF; // галочка

#[derive(Clone, Copy)]
enum UiAction { SelectWallpaper(usize), AddWallpaper }

struct HitRect { x: u32, y: u32, w: u32, h: u32, action: UiAction }

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
    overlay_visible: bool,
    cursor_x: f64,
    cursor_y: f64,
    hit_rects: Vec<HitRect>,
    dirty: bool,
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
        overlay_visible: false,
        cursor_x: -1.0,
        cursor_y: -1.0,
        hit_rects: Vec::new(),
        dirty: false,
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
        config.img = load_image(&config.wallpapers[config.current]);
    } else if let Some(first) = config.wallpapers.first() {
        config.img = load_image(first);
    }

    // Первый раундтрип — получаем все глобалы
    event_queue.roundtrip(&mut config).unwrap();

    // Главный poll-цикл
    loop {
        // Сначала флашим и диспатчим то что есть
        let _ = event_queue.flush();
        let _ = event_queue.dispatch_pending(&mut config);

        // Пробуем приготовиться к чтению wayland
        match event_queue.prepare_read() {
            Some(guard) => {
                let wl_fd = event_queue.as_fd().as_raw_fd();
                let mut fds = [
                    libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: sfd, events: libc::POLLIN, revents: 0 },
                ];

                // Ждём события на любом из FD
                unsafe { libc::poll(fds.as_mut_ptr(), 2, -1); }

                // Wayland
                if fds[0].revents & libc::POLLIN != 0 {
                    guard.read().unwrap_or(0);
                    let _ = event_queue.dispatch_pending(&mut config);
                }

                // Сигналы
                if fds[1].revents & libc::POLLIN != 0 {
                    process_signalfd(&mut config);
                }
            }
            None => {
                // Уже есть отложенные события — обрабатываем
            }
        }

        // Пост-обработка сигналов и dirty-флагов
        if SIG1.swap(false, Ordering::SeqCst) && !config.wallpapers.is_empty() {
            config.current = (config.current + 1) % config.wallpapers.len();
            config.img = load_image(&config.wallpapers[config.current]);
            render_wallpaper(&mut config);
        }
        if SIG2.swap(false, Ordering::SeqCst) {
            config.overlay_visible = !config.overlay_visible;
            if config.overlay_visible {
                load_thumbnails(&mut config);
                build_ui_layout(&mut config);
            }
            config.dirty = true;
        }
        if config.dirty {
            if config.overlay_visible { render_overlay(&mut config); }
            else { hide_overlay(&mut config); }
            config.dirty = false;
        }
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
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => {}
            wl_pointer::Event::Leave { .. } => {
                state.cursor_x = -1.0; state.cursor_y = -1.0; state.dirty = true;
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.cursor_x = surface_x; state.cursor_y = surface_y; state.dirty = true;
            }
            wl_pointer::Event::Button { button: _, state: btn_state, .. }
                if btn_state == WEnum::Value(wl_pointer::ButtonState::Pressed)
                    && state.overlay_visible =>
            {
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
                match hit {
                    Some(UiAction::SelectWallpaper(idx)) => {
                        if idx < state.wallpapers.len() {
                            state.current = idx;
                            state.img = load_image(&state.wallpapers[state.current]);
                            render_wallpaper(state);
                            state.overlay_visible = false;
                            state.dirty = true;
                        }
                    }
                    Some(UiAction::AddWallpaper) => {
                        if let Some(path) = pick_wallpaper_file() {
                            if !state.wallpapers.contains(&path) {
                                state.wallpapers.push(path.clone());
                                save_config(&state.wallpapers);
                                state.current = state.wallpapers.len() - 1;
                                state.img = load_image(&path);
                                render_wallpaper(state);
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
    let Some(ref surface) = state.bg_surface else { return };
    let Some(ref shm) = state.shm else { return };
    let Some(ref qh) = state.qh else { return };
    let (ref _output, w, h) = match state.outputs.first() {
        Some(t) => t.clone(), None => return,
    };
    if w == 0 || h == 0 { return; }

    let pixels = if let Some((iw, ih, ref px)) = state.img {
        let mut rgba = resize(iw, ih, px, w, h);
        for chunk in rgba.chunks_exact_mut(4) { chunk.swap(0, 2); }
        rgba
    } else {
        vec![35u8; (w * h * 4) as usize]
    };

    let buf_size = (w * h * 4) as usize;
    // resize обязан отдать ровно buf_size, но если картинка пришла битой и
    // фолбэк разошёлся с размером выхода — лучше не рисовать кадр, чем упасть
    // на copy_from_slice посреди сессии: обои держат весь фон.
    if pixels.len() < buf_size { return; }
    if let Some((fd, ptr)) = create_shm_fd(buf_size) {
        unsafe {
            std::slice::from_raw_parts_mut(ptr as *mut u8, buf_size).copy_from_slice(&pixels[..buf_size]);
            libc::munmap(ptr, buf_size);
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let pool = shm.create_pool(owned.as_fd(), buf_size as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        drop(buffer); drop(pool);
        surface.commit();
    }
}

// ── Overlay rendering ────────────────────────────────────────────────────────

fn hide_overlay(state: &mut Config) {
    if let Some(ref surface) = state.ol_surface {
        surface.attach(None::<&WlBuffer>, 0, 0);
        surface.commit();
    }
}

fn render_overlay(state: &mut Config) {
    if !state.ol_configured { return; }
    let Some(ref surface) = state.ol_surface else { return };
    let Some(ref shm) = state.shm else { return };
    let Some(ref qh) = state.qh else { return };

    let buf_size = (UI_W * UI_H * 4) as usize;
    let mut pixels = vec![0u8; buf_size];

    // Фон
    fill_rounded_rect(&mut pixels, UI_W, UI_H, 0, 0, UI_W, UI_H, CORNER, BG_COLOR);

    // Карточки — координаты берём из ui_cells, общих с build_ui_layout.
    let n = state.wallpapers.len();
    for (idx, x, y) in ui_cells(n) {
        let hover = is_hover(state, x, y, THUMB, THUMB);
        if idx < n {
            let bg = if hover { HOVER_COLOR } else { CARD_COLOR };
            fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, CORNER, bg);
            if let Some(thumb) = state.thumbnails.get(idx) {
                draw_thumbnail(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, thumb);
            }
            // Индикатор выбранных
            if idx == state.current {
                fill_rounded_rect(&mut pixels, UI_W, UI_H, x + THUMB - 18, y + 6, 12, 12, 6, SEL_COLOR);
            }
        } else {
            let bg = if hover { HOVER_COLOR } else { PLUS_COLOR };
            fill_rounded_rect(&mut pixels, UI_W, UI_H, x, y, THUMB, THUMB, CORNER, bg);
            draw_plus(&mut pixels, UI_W, UI_H, x, y, THUMB);
        }
    }

    // RGBA → BGRA
    for chunk in pixels.chunks_exact_mut(4) { chunk.swap(0, 2); }

    if let Some((fd, ptr)) = create_shm_fd(buf_size) {
        unsafe {
            std::slice::from_raw_parts_mut(ptr as *mut u8, buf_size).copy_from_slice(&pixels[..buf_size]);
            libc::munmap(ptr, buf_size);
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let pool = shm.create_pool(owned.as_fd(), buf_size as i32, qh, ());
        let buffer = pool.create_buffer(0, UI_W as i32, UI_H as i32, (UI_W * 4) as i32, wl_shm::Format::Argb8888, qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, UI_W as i32, UI_H as i32);
        drop(buffer); drop(pool);
        surface.commit();
    }
}

// ── UI helpers ───────────────────────────────────────────────────────────────

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
fn ui_cells(n: usize) -> Vec<(usize, u32, u32)> {
    let mx = (UI_W - COLS * THUMB - (COLS - 1) * GAP) / 2;
    let my = 60; // отступ сверху
    let mut cells = Vec::new();
    // n+1 карточек: сами обои плюс «+» в конце.
    for idx in 0..=n {
        let row = idx as u32 / COLS;
        let col = idx as u32 % COLS;
        let x = mx + col * (THUMB + GAP);
        let y = my + row * (THUMB + GAP);
        if y + THUMB > UI_H { break; }
        cells.push((idx, x, y));
    }
    cells
}

fn build_ui_layout(state: &mut Config) {
    state.hit_rects.clear();
    let n = state.wallpapers.len();
    for (idx, x, y) in ui_cells(n) {
        let action = if idx < n { UiAction::SelectWallpaper(idx) } else { UiAction::AddWallpaper };
        state.hit_rects.push(HitRect { x, y, w: THUMB, h: THUMB, action });
    }
}

fn load_thumbnails(state: &mut Config) {
    state.thumbnails.clear();
    for path in &state.wallpapers {
        if let Some((w, h, px)) = load_image(path) {
            state.thumbnails.push((THUMB, THUMB, resize(w, h, &px, THUMB, THUMB)));
        } else {
            state.thumbnails.push((THUMB, THUMB, vec![60u8; (THUMB * THUMB * 4) as usize]));
        }
    }
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
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "gif" => {
            let file = std::fs::File::open(path).ok()?;
            let mut d = gif::DecodeOptions::new();
            d.set_color_output(gif::ColorOutput::RGBA);
            let mut r = d.read_info(file).ok()?;
            let f = r.read_next_frame().ok()??;
            Some((f.width as u32, f.height as u32, f.buffer.to_vec()))
        }
        _ => {
            image::open(path).ok().map(|img| { let (w, h) = img.dimensions(); (w, h, img.to_rgba8().into_raw()) })
        }
    }
}

/// Возвращает картинку, растянутую под выход. Пустой/битый вход не роняет
/// клиент: from_raw отдаёт None, если длина буфера не сходится с iw*ih*4.
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

fn is_image(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif"
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

fn save_config(wallpapers: &[String]) {
    let path = format!("{}/.config/dwall/wallpapers.json", home());
    if let Some(d) = std::path::Path::new(&path).parent() { let _ = std::fs::create_dir_all(d); }
    if let Ok(data) = serde_json::to_string_pretty(wallpapers) { let _ = std::fs::write(&path, &data); }
}
