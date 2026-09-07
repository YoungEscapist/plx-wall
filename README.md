# plx-wall

Wallpapers for [Parallax](https://github.com/YoungEscapist/parallax-wm) — and for
any other `wlr-layer-shell` compositor.

It is a layer-shell client, not a desktop shell: it paints the background of
every output, carries a picker you drive with the mouse, plays live wallpapers
straight from `libavcodec` (hardware decoding into a dmabuf where the GPU allows
it), and derives a colour palette from whatever is on screen so the rest of the
system matches the wallpaper.

Parallax ships with it as a dependency — `install.sh` there builds and installs
it — but it stands on its own and works under sway, river, Hyprland and anything
else that speaks `zwlr_layer_shell_v1`.

## What it does

- **Still and live wallpapers.** JPEG/PNG/… through `image`, GIF, and video
  (mp4, webm) decoded in-process with libavcodec. Video goes zero-copy into a
  dmabuf via VAAPI when the driver supports it, and falls back to software
  otherwise. A live wallpaper on an idle screen sleeps: no frames are pushed
  when nothing has asked for one.
- **A picker.** Toggle the overlay and browse your wallpapers as a ribbon of
  cards with depth, scroll it, hit “next” to keep going. Cards are scaled
  bilinearly, thumbnails are cached in `~/.cache/plx-wall/thumbs`.
- **Online sources.** wallhaven, Openverse, Bing's daily UHD shot, picsum,
  Wikimedia Commons, NASA's image library and the Art Institute of Chicago,
  plus live wallpapers from the same picker. The last two image sources
  are effectively bottomless (164 714 and 132 740 items), which is what the
  endless ribbon wants; the others run out fast. No API keys.
- **A palette for everything else.** Every wallpaper change recomputes a
  16-colour ANSI palette plus foreground/background/accent, writes it to
  `~/.cache/plx-wall/palette.{json,sh}` and `~/.config/ghostty/colors`, themes
  `fuzzel`, and repaints *already open* terminals through OSC 4/10/11/12 —
  no restart, no reload. Parallax reads the same accent for its window glow.
  `~/.local/bin/plx-wall-themes`, if you have one, is run for the applications
  that cannot take a terminal palette.

## Install

```sh
git clone https://github.com/YoungEscapist/plx-wall.git
cd plx-wall
./install.sh
```

The script installs the system packages (Void, Arch, Debian/Ubuntu, Fedora),
checks for Rust, builds with `--release` and puts the binary in
`~/.local/bin/plx-wall`. Every command is printed before it runs, and anything
needing root is asked about first. `--help` lists the flags; `--dry-run` shows
what would happen and does nothing.

Or without cloning first:

```sh
curl -fsSL https://raw.githubusercontent.com/YoungEscapist/plx-wall/master/install.sh | bash
```

By hand: `cargo build --release && install -Dm755 target/release/plx-wall ~/.local/bin/plx-wall`.

**Dependencies.** pkg-config, a C compiler, clang (bindgen reads the ffmpeg
headers of *your* system — see `build.rs`), the wayland client libraries, and
ffmpeg's dev packages (libavcodec, libavformat, libavutil, libswscale). At
runtime it also uses `curl` for the online sources and `ffmpeg`/`ffprobe` for
thumbnails of videos. On NixOS: `nix-shell`, then `cargo build --release`.

## Use

```sh
plx-wall                       # start; restores the last wallpaper
plx-wall ~/Pictures/sky.png    # start with this one and remember it
plx-wall --palette [file]      # recompute the palette and repaint terminals
plx-wall --online images space # ask the sources and print what they found
                               # (`images`/`live`, then filters: размер=, цвет=…)
plx-wall --hwtest video.mp4    # check VAAPI → dmabuf on this machine
```

Running it needs a compositor: it is a layer-shell client and expects
`WAYLAND_DISPLAY` to point at one.

Once it runs, two signals drive it — bind them to keys in your compositor:

```
pkill -USR1 -x plx-wall     # next wallpaper
pkill -USR2 -x plx-wall     # show/hide the picker
```

In Parallax's `config.lua` (`Super+W` is already `toggle_floating`, hence Alt):

```lua
bind{ mods = "alt",       key = "w", action = "spawn", cmd = "pkill -USR1 -x plx-wall" }
bind{ mods = "alt+shift", key = "w", action = "spawn", cmd = "pkill -USR2 -x plx-wall" }
```

State lives in `~/.config/plx-wall`: `wallpapers.json` (the list), `current`
(what is showing), `downloaded/` (what the online sources brought). Frame timing
can be watched with `PLX_WALL_PROFILE=1`, and the frame rate of live wallpapers
capped with `PLX_WALL_FPS`.

## Licence

GPL-3.0-or-later, the same as Parallax. See `LICENSE`.
