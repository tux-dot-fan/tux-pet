#[macro_use]
mod shared;
mod video;
use std::collections::HashMap;
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::accept;
use shared::{AnimationKind, BehaviorType, CharacterDef, PetConfig};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _, SK};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

mod menu;
mod settings;
use menu::ContextMenu;

#[derive(Debug, serde::Deserialize)]
struct WaylandWindow {
    #[allow(dead_code)]
    title: String,
    x: i32,
    y: i32,
    w: i32,
    #[allow(dead_code)]
    h: i32,
}

fn fetch_windows_dbus() -> Option<Vec<Surface>> {
    use zbus::blocking::Connection as ZConn;
    let conn = ZConn::session().ok()?;
    let reply = conn.call_method(
        Some("org.tux.WindowTracker"),
        "/org/tux/WindowTracker",
        Some("org.tux.WindowTracker"),
        "GetWindows",
        &(),
    ).ok()?;
    let json: String = reply.body().deserialize().ok()?;
    let wins: Vec<WaylandWindow> = serde_json::from_str(&json).ok()?;
    Some(wins.into_iter()
        .filter(|w| w.w > 50)
        .map(|w| Surface { x: w.x, y: w.y, w: w.w })
        .collect())
}

fn start_dbus_watcher(surfaces: Arc<Mutex<Vec<Surface>>>, dirty: Arc<Mutex<bool>>) {
    thread::spawn(move || {
        use zbus::blocking::{Connection as ZConn, MessageIterator};
        let conn = match ZConn::session() {
            Ok(c) => c,
            Err(_) => return,
        };
        let rule = "type='signal',interface='org.tux.WindowTracker',member='WindowsChanged'";
        if conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "AddMatch",
            &rule,
        ).is_err() { return; }

        tux_log!("[pet] D-Bus WindowTracker watcher started");

        let iter = MessageIterator::from(&conn);
        for msg in iter {
            let Ok(msg) = msg else { continue };
            if msg.member().as_deref() != Some("WindowsChanged") { continue }

            if let Some(wins) = fetch_windows_dbus() {
                tux_log!("[pet] dbus surfaces: {} windows", wins.len());
                *surfaces.lock().unwrap() = wins;
                *dirty.lock().unwrap() = true;
            }
        }
    });
}

use video::VideoPlayer;

const PET_W: u16 = 128;
const PET_H: u16 = 128;
const WIN_SIZE_MAX: u16 = 1280;
const TICK_MS: u64 = 40;
const GRAVITY: f64 = 0.5;
const WALK_SPEED: f64 = 1.8;

#[derive(Clone, PartialEq)]
struct Surface {
    x: i32,
    y: i32,
    w: i32,
}

struct Pet {
    x: f64,
    y: f64,
    vel_y: f64,
    frame: u32,
    scale: f64,
    target_scale: f64,
    grounded: bool,
    shake_phase: f64,
    jump_vel: f64,
    seq_step: usize,
    seq_tick: u32,
    seq_vel_y: f64,
}

struct LoadedAnimation {
    frames: Vec<Vec<u8>>,
    ticks_per_frame: u32,
}

struct LoadedCharacter {
    id: String,
    animations: HashMap<String, LoadedAnimation>,
}

impl LoadedCharacter {
    fn get_anim<'a>(&'a self, id: &str) -> Option<&'a LoadedAnimation> {
        self.animations.get(id)
            .or_else(|| self.animations.get("idle"))
            .or_else(|| self.animations.values().next())
    }
}

fn load_all_characters() -> Vec<LoadedCharacter> {
    shared::all_characters().iter().map(|char_def| {
        let mut animations = HashMap::new();
        for anim_def in &char_def.animations {
            if let shared::AnimationKind::Frames { frames: frame_paths, ticks_per_frame } = &anim_def.kind {
                let frames: Vec<Vec<u8>> = frame_paths.iter()
                    .filter_map(|p| std::fs::read(p).ok())
                    .collect();
                if !frames.is_empty() {
                    animations.insert(anim_def.id.clone(), LoadedAnimation {
                        frames,
                        ticks_per_frame: *ticks_per_frame,
                    });
                }
            }
        }
        LoadedCharacter { id: char_def.id.clone(), animations }
    }).collect()
}

fn get_loaded_char<'a>(chars: &'a [LoadedCharacter], char_id: &str) -> &'a LoadedCharacter {
    chars.iter().find(|c| c.id == char_id).unwrap_or(&chars[0])
}

fn get_override_anim<'a>(chars: &'a [LoadedCharacter], config: &shared::PetConfig) -> Option<&'a LoadedAnimation> {
    let override_ids = ["blink", "play", "chase"];
    if override_ids.contains(&config.animation.as_str()) {
        let lc = get_loaded_char(chars, &config.character);
        lc.get_anim(&config.animation)
    } else {
        None
    }
}

fn anim_id_for_config<'a>(chars: &'a [LoadedCharacter], config: &shared::PetConfig) -> Option<&'a LoadedAnimation> {
    let lc = get_loaded_char(chars, &config.character);
    lc.get_anim(&config.animation)
}

fn render_svg(svg_data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let tree = resvg::usvg::Tree::from_data(svg_data, &resvg::usvg::Options::default())
        .unwrap();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).unwrap();
    pixmap.fill(resvg::tiny_skia::Color::TRANSPARENT);
    let scale_x = width as f32 / tree.size().width();
    let scale_y = height as f32 / tree.size().height();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale_x, scale_y), &mut pixmap.as_mut());
    pixmap.data().to_vec()
}

fn pet_pos_path() -> Option<std::path::PathBuf> {
    let mut p = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    p.push(".config/tux");
    let _ = std::fs::create_dir_all(&p);
    p.push("pet_pos.json");
    Some(p)
}

fn load_pet_pos() -> Option<(f64, f64, f64)> {
    let text = std::fs::read_to_string(pet_pos_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let x = v.get("x")?.as_f64()?;
    let y = v.get("y")?.as_f64()?;
    let scale = v.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
    Some((x, y, scale))
}

fn save_pet_pos(x: f64, y: f64, scale: f64) {
    if let Some(p) = pet_pos_path() {
        let v = serde_json::json!({"x": x as i32, "y": y as i32, "scale": (scale * 10.0).round() / 10.0});
        if let Ok(s) = serde_json::to_string(&v) {
            let _ = std::fs::write(p, s);
        }
    }
}

fn kill_existing_instance() {
    use std::process::Command;
    let my_pid = std::process::id();
    if let Ok(out) = Command::new("pgrep").arg("-x").arg("tux-pet").output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                if pid != my_pid {
                    tux_log!("[pet] killing old instance pid={}", pid);
                    let _ = Command::new("kill").arg(pid.to_string()).output();
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tux_log!("[pet] starting...");
    let assets_dir = std::env::var("TUX_ASSETS").unwrap_or_else(|_| "not_set".to_string());
    tux_log!("[pet] TUX_ASSETS={}", assets_dir);
    let pets = shared::pets_dir();
    tux_log!("[pet] pets_dir={}", pets.display());
    let chars = shared::all_characters();
    tux_log!("[pet] loaded {} characters", chars.len());
    for c in chars.iter() {
        tux_log!("[pet]   {} ({} animations)", c.id, c.animations.len());
    }

    kill_existing_instance();
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];

    let (work_x, work_y, work_w, work_h) = get_workarea(&conn, screen.root)
        .unwrap_or((0, 0, screen.width_in_pixels as i32, screen.height_in_pixels as i32));
    let screen_w = work_w;
    let screen_h = work_y + work_h;

    let win = conn.generate_id()?;

    let visual = find_argb_visual(&conn, screen_num);
    let (depth, visual_id) = match visual {
        Some(v) => v,
        None => (screen.root_depth, screen.root_visual),
    };

    let colormap = if depth == 32 {
        let cmap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, cmap, screen.root, visual_id)?;
        cmap
    } else {
        screen.default_colormap
    };

    let initial_size: u16 = PET_W;
    conn.create_window(
        depth,
        win,
        screen.root,
        (screen_w / 2) as i16,
        200,
        initial_size,
        initial_size,
        0,
        WindowClass::INPUT_OUTPUT,
        visual_id,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON1_MOTION),
    )?;

    set_empty_input_shape(&conn, win)?;

    conn.map_window(win)?;
    conn.flush()?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;

    let mut backing = conn.generate_id()?;
    conn.create_pixmap(depth, backing, win, initial_size, initial_size)?;
    let mut cur_win_w: u16 = initial_size;
    let mut cur_win_h: u16 = initial_size;

    let characters = load_all_characters();
    let saved_state = load_pet_pos();
    let init_scale = saved_state.map(|(_, _, s)| s).unwrap_or(1.0);
    let config: Arc<Mutex<shared::PetConfig>> = Arc::new(Mutex::new(shared::PetConfig { base_scale: init_scale, ..shared::PetConfig::default() }));

    {
        let config: Arc<Mutex<shared::PetConfig>> = Arc::clone(&config);
        thread::spawn(move || {
            let server = TcpListener::bind("127.0.0.1:9872").expect("Failed to bind WS port");
            tux_log!("[pet] WS server listening on {}", "127.0.0.1:9872");
            for stream in server.incoming().flatten() {
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let config: Arc<Mutex<shared::PetConfig>> = Arc::clone(&config);
                thread::spawn(move || {
                    if let Ok(mut ws) = accept(stream) {
                        while let Ok(msg) = ws.read() {
                            match msg {
                                tungstenite::Message::Text(text) => {
                                    if let Ok(new_cfg) = serde_json::from_str::<shared::PetConfig>(&text) {
                                        tux_log!("[pet] received config: {:?} / {:?}", new_cfg.character, new_cfg.animation);
                                        *config.lock().unwrap() = new_cfg;
                                    } else {
                                        tux_log!("[pet] parse error: {}", text);
                                    }
                                }
                                tungstenite::Message::Close(_) => {
                                    tux_log!("[pet] client disconnected");
                                    break;
                                }
                                _ => {}
                            }
                        }
                    } else {
                        tux_log!("[pet] accept failed");
                    }
                });
            }
        });
    }

    let panel_w = 300i32;
    let panel_margin_left = 8i32;
    let panel_margin_top = 40i32;
    let default_x = (work_x + panel_margin_left + panel_w + 20) as f64;
    let default_y = (work_y + panel_margin_top + 200) as f64;
    let (init_x, init_y, _) = saved_state.unwrap_or((default_x, default_y, 1.0));

    let mut pet = Pet {
        x: init_x.clamp(0.0, (screen_w - PET_W as i32) as f64),
        y: init_y.clamp(0.0, (screen_h - PET_H as i32) as f64),
        vel_y: 0.0,
        frame: 0,
        scale: init_scale,
        target_scale: init_scale,
        grounded: false,
        shake_phase: 0.0,
        jump_vel: 0.0,
        seq_step: 0,
        seq_tick: 0,
        seq_vel_y: 0.0,
    };

    let mut pet_state = shared::load_pet_state();
    let mut state_save_counter: u64 = 0;
    let mut last_auto_anim_switch: u64 = 0;

    let mut last_config = shared::PetConfig::default();
    let mut video_player: Option<VideoPlayer> = None;
    let mut video_last_frame = Instant::now();
    let mut first_render = true;
    let mut drag: Option<(i16, i16, f64, f64)> = None;
    let mut last_input_shape: Option<(i16, i16, u16, u16)> = None;

    let all_chars: Vec<_> = shared::all_characters().iter().map(|c| c.name.clone()).collect();
    let cur_char = config.lock().unwrap().character.clone();
    let cur_scale = config.lock().unwrap().base_scale;
    let char_infos: Vec<settings::CharInfo> = shared::all_characters().iter().map(|c| {
        let anim_paths: Vec<String> = c.animations.iter().map(|a| match &a.kind {
            shared::AnimationKind::Video { path } => path.clone(),
            shared::AnimationKind::Frames { frames, .. } => frames.first().cloned().unwrap_or_default(),
        }).collect();
        let anim_ticks_per_frame: Vec<u32> = c.animations.iter().map(|a| match &a.kind {
            shared::AnimationKind::Video { .. } => 6,
            shared::AnimationKind::Frames { ticks_per_frame, .. } => *ticks_per_frame,
        }).collect();
        settings::CharInfo {
            id: c.id.clone(), name: c.name.clone(), avatar_path: c.avatar_path.clone(),
            anim_ids: c.animations.iter().map(|a| a.id.clone()).collect(),
            anim_names: c.animations.iter().map(|a| a.name.clone()).collect(),
            anim_paths,
            anim_ticks_per_frame,
        }
    }).collect();
    let menu_items = vec![
        menu::MenuItem { label: "设置宠物", id: "settings" },
        menu::MenuItem { label: "关闭宠物", id: "quit" },
    ];
    let mut ctx_menu = ContextMenu::new(&conn, screen, depth, visual_id, colormap, menu_items)?;
    let mut settings_win = settings::SettingsWindow::new(&conn, screen, depth, visual_id, colormap, char_infos)?;
    settings_win.scale = init_scale;
    let mut menu_visible = false;

    let dbus_surfaces: Arc<Mutex<Vec<Surface>>> = Arc::new(Mutex::new(Vec::new()));
    let dbus_dirty: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let has_dbus = fetch_windows_dbus().map(|wins| {
        *dbus_surfaces.lock().unwrap() = wins;
        true
    }).unwrap_or(false);

    if has_dbus {
        tux_log!("[pet] using D-Bus WindowTracker for surfaces");
        start_dbus_watcher(dbus_surfaces.clone(), dbus_dirty.clone());
    } else {
        tux_log!("[pet] D-Bus WindowTracker unavailable, using X11 scan");
        conn.change_window_attributes(
            screen.root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?;
        conn.flush()?;
        *dbus_surfaces.lock().unwrap() = scan_surfaces(&conn, screen.root, win);
    }

    let mut surfaces_dirty_at: Option<std::time::Instant> = None;
    const SURFACES_DEBOUNCE_MS: u64 = 200;

    loop {
        let tick_start = Instant::now();

        while let Some(event) = conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::ButtonPress(ev) if ev.detail == 1 => {
                    if ev.event == ctx_menu.win {
                        let item_idx = ctx_menu.hit_test(ev.event_y);
                        ctx_menu.hide(&conn)?;
                        menu_visible = false;
                        if let Some(idx) = item_idx {
                            match ctx_menu.items[idx].id {
                                "quit" => std::process::exit(0),
                                "settings" => {
                                    menu_visible = false;
                                    ctx_menu.hide(&conn)?;
                                    let cur_char = config.lock().unwrap().character.clone();
                                    let cur_sel = shared::all_characters().iter().position(|c| c.id == cur_char).unwrap_or(0);
                                    let cur_scale = config.lock().unwrap().base_scale;
                                    settings_win.sel_char = cur_sel;
                                    settings_win.scale = cur_scale;
                                    let _ = settings_win.show(&conn, ev.root_x, ev.root_y, screen_w as i16, screen_h as i16);
                                }
                                _ => {}
                            }
                        }
                    } else if settings_win.visible && ev.event == settings_win.win {
                        let hit = settings_win.hit_test(ev.event_x as f64, ev.event_y as f64);
                        match hit {
                            settings::Hit::Close => {
                                let _ = settings_win.hide(&conn);
                            }
                            settings::Hit::SelectChar(i) => {
                                settings_win.sel_char = i;
                                settings_win.sel_anim = 0;
                                if let Some(c) = shared::all_characters().get(i) {
                                    config.lock().unwrap().character = c.id.clone();
                                    config.lock().unwrap().animation = c.animations.first().map(|a| a.id.clone()).unwrap_or_else(|| "idle".into());
                                }
                                let _ = settings_win.render(&conn);
                            }
                            settings::Hit::SelectAnim(i) => {
                                settings_win.sel_anim = i;
                                if let Some(c) = shared::all_characters().get(settings_win.sel_char) {
                                    if let Some(a) = c.animations.get(i) {
                                        config.lock().unwrap().animation = a.id.clone();
                                    }
                                }
                                let _ = settings_win.render(&conn);
                            }
                            settings::Hit::Slider(frac) => {
                                settings_win.scale = (frac * 9.7 + 0.3).clamp(0.3, 10.0);
                                config.lock().unwrap().base_scale = settings_win.scale;
                                settings_win.dragging_slider = true;
                                let _ = settings_win.render(&conn);
                            }
                            settings::Hit::None => {}
                        }
                    } else if menu_visible {
                        ctx_menu.hide(&conn)?;
                        menu_visible = false;
                    } else if settings_win.visible {
                        let _ = settings_win.hide(&conn);
                    } else if ev.event == win {
                        drag = Some((ev.root_x, ev.root_y, pet.x, pet.y));
                        pet.vel_y = 0.0;
                    }
                }
                x11rb::protocol::Event::ButtonPress(ev) if ev.detail == 4 || ev.detail == 5 => {
                    if settings_win.visible && ev.event == settings_win.win {
                        let delta = if ev.detail == 4 { -1 } else { 1 };
                        let mx = ev.event_x as f64;
                        if mx < 130.0 { settings_win.scroll_char(delta); }
                        else { settings_win.scroll_anim(delta); }
                        let _ = settings_win.render(&conn);
                    }
                }
                x11rb::protocol::Event::ButtonPress(ev) if ev.detail == 3 => {
                    if ev.event == win {
                        if menu_visible {
                            ctx_menu.hide(&conn)?;
                            menu_visible = false;
                        } else {
                            ctx_menu.show(&conn, ev.root_x, ev.root_y)?;
                            menu_visible = true;
                        }
                    } else if ev.event == ctx_menu.win {
                        ctx_menu.hide(&conn)?;
                        menu_visible = false;
                    }
                }
                x11rb::protocol::Event::ButtonRelease(ev) if ev.detail == 1 => {
                    if settings_win.dragging_slider {
                        save_pet_pos(pet.x, pet.y, config.lock().unwrap().base_scale);
                    }
                    settings_win.dragging_slider = false;
                    if drag.take().is_some() {
                        if has_dbus {
                            if let Some(wins) = fetch_windows_dbus() {
                                *dbus_surfaces.lock().unwrap() = wins;
                            }
                        } else {
                            *dbus_surfaces.lock().unwrap() = scan_surfaces(&conn, screen.root, win);
                        }
                        let surfs = dbus_surfaces.lock().unwrap().clone();
                        let foot_y = pet.y + PET_H as f64;
                        let on_surface = find_surface_at(pet.x, foot_y + 2.0, &surfs);
                        let at_floor = pet.y >= (screen_h - PET_H as i32) as f64 - 5.0;
                        if on_surface.is_some() || at_floor {
                            pet.grounded = true;
                            pet.vel_y = 0.0;
                        } else if let Some(s) = find_surface_below(pet.x, pet.y, &surfs) {
                            pet.y = (s.y - PET_H as i32) as f64;
                            pet.vel_y = 0.0;
                            pet.grounded = true;
                        } else {
                            pet.grounded = false;
                            pet.vel_y = 0.0;
                        }
                        save_pet_pos(pet.x, pet.y, config.lock().unwrap().base_scale);
                    }
                }
                x11rb::protocol::Event::MotionNotify(ev) => {
                    if settings_win.visible && settings_win.dragging_slider && ev.event == settings_win.win {
                        let slider_x = 12.0 + 130.0 + 1.0 + 16.0f64;
                        let slider_w = 500.0 - 130.0 - 1.0 - 32.0f64;
                        let frac = ((ev.event_x as f64 - slider_x) / slider_w).clamp(0.0, 1.0);
                        settings_win.scale = (frac * 9.7 + 0.3).clamp(0.3, 10.0);
                        config.lock().unwrap().base_scale = settings_win.scale;
                        let _ = settings_win.render(&conn);
                    } else if ev.event == ctx_menu.win {
                        let idx = ctx_menu.hit_test(ev.event_y);
                        ctx_menu.set_hovered(&conn, idx)?;
                    } else if let Some((sx, sy, px, py)) = drag {
                        pet.x = (px + (ev.root_x - sx) as f64).clamp(0.0, (screen_w - PET_W as i32) as f64);
                        pet.y = (py + (ev.root_y - sy) as f64).clamp(0.0, (screen_h - PET_H as i32) as f64);
                    }
                }
                x11rb::protocol::Event::LeaveNotify(ev) if ev.event == ctx_menu.win => {
                    ctx_menu.set_hovered(&conn, None)?;
                }
                x11rb::protocol::Event::ConfigureNotify(ev) if !has_dbus => {
                    if ev.window != win && ev.event != win {
                        surfaces_dirty_at = Some(std::time::Instant::now());
                    }
                }
                x11rb::protocol::Event::MapNotify(ev) if !has_dbus => {
                    if ev.window != win { surfaces_dirty_at = Some(std::time::Instant::now()); }
                }
                x11rb::protocol::Event::UnmapNotify(ev) if !has_dbus => {
                    if ev.window != win { surfaces_dirty_at = Some(std::time::Instant::now()); }
                }
                x11rb::protocol::Event::DestroyNotify(ev) if !has_dbus => {
                    if ev.window != win { surfaces_dirty_at = Some(std::time::Instant::now()); }
                }
                _ => {}
            }
        }

        if !has_dbus {
            if let Some(t) = surfaces_dirty_at {
                if t.elapsed().as_millis() >= SURFACES_DEBOUNCE_MS as u128 {
                    let new = scan_surfaces(&conn, screen.root, win);
                    let mut guard = dbus_surfaces.lock().unwrap();
                    if new != *guard {
                        tux_log!("[pet] X11 surfaces updated: {} windows", new.len());
                    }
                    *guard = new;
                    surfaces_dirty_at = None;
                }
            }
        } else if *dbus_dirty.lock().unwrap() {
            *dbus_dirty.lock().unwrap() = false;
            let surfs = dbus_surfaces.lock().unwrap();
            tux_log!("[pet] dbus surfaces: {} windows", surfs.len());
            for s in surfs.iter() {
                tux_log!("[pet]   x={} y={} w={}", s.x, s.y, s.w);
            }
        }

        if settings_win.visible && (settings_win.has_video_preview() || settings_win.has_frame_preview()) {
            let _ = settings_win.render(&conn);
        }

        let current_config = config.lock().unwrap().clone();
        let current_surfaces = dbus_surfaces.lock().unwrap().clone();
        let config_changed = last_config != current_config;
        let anim_changed = last_config.character != current_config.character
            || last_config.animation != current_config.animation;
        if config_changed {
            tux_log!("[pet] config changed -> {}/{} (pos={:.0},{:.0})",
                current_config.character, current_config.animation,
                pet.x, pet.y);
            last_config = current_config.clone();
        }

        pet_state.tick(false);
        state_save_counter += 1;
        if state_save_counter >= 500 {
            shared::save_pet_state(&pet_state);
            state_save_counter = 0;
        }

        if pet_state.animation_ticks_left == 0 && last_auto_anim_switch > 0 {
            last_auto_anim_switch = 0;
            // Validate animation exists for current character before applying
            let current_char_def = shared::all_characters()
                .iter()
                .find(|c| c.id == current_config.character);
            if let Some(c) = current_char_def {
                if c.animations.iter().any(|a| a.id == pet_state.current_animation) {
                    config.lock().unwrap().animation = pet_state.current_animation.clone();
                }
            }
        }

        if pet_state.animation_ticks_left == 0 && last_auto_anim_switch == 0 {
            let current_hour = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() / 3600 % 24;

            let current_char = shared::all_characters()
                .iter()
                .find(|c| c.id == current_config.character);

            if let Some(char_def) = current_char {
                if let Some(new_anim) = shared::select_animation(&pet_state, &char_def.rules, current_hour as u32) {
                    if new_anim != current_config.animation && char_def.animations.iter().any(|a| a.id == new_anim) {
                        tux_log!("[pet] auto-switching to: {} (hunger={:.1}, mood={:.1}, energy={:.1})",
                            new_anim, pet_state.hunger, pet_state.mood, pet_state.energy);
                        pet_state.current_animation = new_anim.clone();
                        let anim_def = char_def.animations.iter().find(|a| a.id == new_anim);
                        if let Some(_ad) = anim_def {
                            pet_state.animation_ticks_left = if let Some(rule) = char_def.rules.iter().find(|r| r.animation == new_anim) {
                                rule.duration_ticks
                            } else {
                                300
                            };
                        }
                        config.lock().unwrap().animation = new_anim;
                        last_auto_anim_switch = pet_state.idle_ticks;
                    }
                }
            }
        }

        let current_anim_def = shared::all_characters()
            .iter()
            .find(|c| c.id == current_config.character)
            .and_then(|c| c.animations.iter().find(|a| a.id == current_config.animation));

        let behavior = current_anim_def
            .map(|a| &a.behavior)
            .cloned()
            .unwrap_or_default();

        let is_video = matches!(&current_anim_def, Some(a) if matches!(a.kind, shared::AnimationKind::Video { .. }));

        use shared::BehaviorType::*;
        let at_floor = pet.y >= (screen_h - PET_H as i32) as f64 - 5.0;

        match behavior.behavior_type {
            Fixed => {
                pet.vel_y = 0.0;
            }
            shared::BehaviorType::Idle | shared::BehaviorType::Fall => {
                if !pet.grounded && !at_floor {
                    pet.vel_y += GRAVITY;
                    pet.y += pet.vel_y;
                    if let Some(s) = find_surface_below(pet.x, pet.y, &current_surfaces) {
                        pet.y = (s.y - PET_H as i32) as f64;
                        pet.vel_y = 0.0;
                        pet.grounded = true;
                    } else if pet.y >= (screen_h - PET_H as i32) as f64 {
                        pet.y = (screen_h - PET_H as i32) as f64;
                        pet.vel_y = 0.0;
                        pet.grounded = true;
                    }
                } else {
                    let on_surface = find_surface_at(pet.x, pet.y + PET_H as f64 + 2.0, &current_surfaces);
                    if on_surface.is_none() && !at_floor {
                        pet.grounded = false;
                        pet.vel_y = 0.0;
                    }
                }
            }
            shared::BehaviorType::WalkLeft | shared::BehaviorType::WalkRight | shared::BehaviorType::RunLeft | shared::BehaviorType::RunRight => {
                let speed = behavior.speed;
                let dx = match behavior.behavior_type {
                    shared::BehaviorType::WalkLeft | shared::BehaviorType::RunLeft  => -speed,
                    _                   =>  speed,
                };
                pet.x += dx;

                if !pet.grounded && !at_floor {
                    pet.vel_y += GRAVITY;
                    pet.y += pet.vel_y;
                    if let Some(s) = find_surface_below(pet.x, pet.y, &current_surfaces) {
                        pet.y = (s.y - PET_H as i32) as f64;
                        pet.vel_y = 0.0;
                        pet.grounded = true;
                    } else if pet.y >= (screen_h - PET_H as i32) as f64 {
                        pet.y = (screen_h - PET_H as i32) as f64;
                        pet.vel_y = 0.0;
                        pet.grounded = true;
                    }
                } else {
                    let on_surface = find_surface_at(pet.x, pet.y + PET_H as f64 + 2.0, &current_surfaces);
                    if on_surface.is_none() && !at_floor {
                        pet.grounded = false;
                        pet.vel_y = 0.0;
                    } else if let Some(s) = on_surface {
                        let left = s.x as f64;
                        let right = (s.x + s.w) as f64 - PET_W as f64;
                        pet.x = pet.x.clamp(left, right);
                    }
                }

                if pet.x <= 0.0 { pet.x = 0.0; }
                if pet.x >= (screen_w - PET_W as i32) as f64 { pet.x = (screen_w - PET_W as i32) as f64; }
            }
            ClimbUp => {
                pet.y -= behavior.speed;
                pet.y = pet.y.max(0.0);
                pet.grounded = false;
                pet.vel_y = 0.0;
            }
            ClimbDown => {
                pet.y += behavior.speed;
                if at_floor || find_surface_at(pet.x, pet.y + PET_H as f64 + 2.0, &current_surfaces).is_some() {
                    pet.grounded = true;
                }
                pet.vel_y = 0.0;
            }
            Jump => {
                if pet.grounded || at_floor {
                    pet.jump_vel = -12.0;
                    pet.grounded = false;
                }
                pet.vel_y = pet.jump_vel;
                pet.jump_vel += GRAVITY;
                pet.y += pet.vel_y;
                if let Some(s) = find_surface_below(pet.x, pet.y, &current_surfaces) {
                    pet.y = (s.y - PET_H as i32) as f64;
                    pet.vel_y = 0.0;
                    pet.jump_vel = 0.0;
                    pet.grounded = true;
                } else if pet.y >= (screen_h - PET_H as i32) as f64 {
                    pet.y = (screen_h - PET_H as i32) as f64;
                    pet.vel_y = 0.0;
                    pet.jump_vel = 0.0;
                    pet.grounded = true;
                }
            }
            FollowCursor => {
                pet.vel_y = 0.0;
                pet.grounded = false;
            }
            Shake => {
                pet.shake_phase += 0.4;
                let offset = (pet.shake_phase.sin() * 6.0) as f64;
                pet.x = (pet.x + offset).clamp(0.0, (screen_w - PET_W as i32) as f64);
                if !pet.grounded && !at_floor {
                    pet.vel_y += GRAVITY;
                    pet.y += pet.vel_y;
                    if pet.y >= (screen_h - PET_H as i32) as f64 {
                        pet.y = (screen_h - PET_H as i32) as f64;
                        pet.vel_y = 0.0;
                        pet.grounded = true;
                    }
                }
            }
            Sequence => {
                let steps = &behavior.steps;
                if !steps.is_empty() {
                    let step_idx = pet.seq_step.min(steps.len() - 1);
                    let step = &steps[step_idx];
                    let is_last = pet.seq_step >= steps.len() - 1;

                    if step.gravity {
                        pet.seq_vel_y += GRAVITY;
                    }
                    let vy = if step.gravity { pet.seq_vel_y } else { step.move_y };

                    pet.x = (pet.x + step.move_x).clamp(0.0, (screen_w - PET_W as i32) as f64);
                    pet.y += vy;

                    if pet.y >= (screen_h - PET_H as i32) as f64 {
                        pet.y = (screen_h - PET_H as i32) as f64;
                        pet.seq_vel_y = 0.0;
                        pet.grounded = true;
                    } else if pet.y <= 0.0 {
                        pet.y = 0.0;
                        pet.seq_vel_y = 0.0;
                    } else {
                        pet.grounded = false;
                    }

                    pet.seq_tick += 1;
                    if pet.seq_tick >= step.duration {
                        pet.seq_tick = 0;
                        pet.seq_vel_y = 0.0;
                        if is_last {
                            if behavior.loop_sequence {
                                pet.seq_step = 0;
                            }
                        } else {
                            pet.seq_step += 1;
                            let next = &steps[pet.seq_step];
                            pet.seq_vel_y = if next.gravity { next.move_y } else { 0.0 };
                        }
                    }
                }
            }
        }

        pet.target_scale = 1.0;

        if anim_changed {
            pet.shake_phase = 0.0;
            pet.jump_vel = 0.0;
            pet.seq_step = 0;
            pet.seq_tick = 0;
            pet.seq_vel_y = 0.0;
            if matches!(behavior.behavior_type, shared::BehaviorType::Fixed) {
                pet.vel_y = 0.0;
            }
        }
        let scale_changed = is_video && config_changed && !anim_changed;
        let need_video_init = is_video && (anim_changed || scale_changed || (first_render && video_player.is_none()));
        if need_video_init {
            video_player = None;
            if let Some(shared::AnimationKind::Video { path }) = current_anim_def.map(|a| a.kind.clone()) {
                let max_dim = (PET_W as f64 * current_config.base_scale).min(WIN_SIZE_MAX as f64 * 0.99) as u32;
                match VideoPlayer::open_fit(&path, max_dim) {
                    Some(p) => {
                        tux_log!("[pet] opened video {}", path);
                        video_player = Some(p);
                        video_last_frame = Instant::now();
                    }
                    None => tux_log!("[pet] failed to open video {}", path),
                }
            }
        } else if anim_changed {
            conn.flush()?;
        }
        first_render = false;

        if is_video {
            pet.target_scale = current_config.base_scale;
            pet.scale = current_config.base_scale;
            pet.frame += 1;

            if let Some(ref mut vp) = video_player {
                let frame_interval = Duration::from_millis(33);
                if video_last_frame.elapsed() >= frame_interval {
                    video_last_frame = Instant::now();

                    let vid_w = vp.width;
                    let vid_h = vp.height;
                    if let Some(rgba) = vp.next_frame() {
                        let vw = vid_w as i32;
                        let vh = vid_h as i32;
                        let win_x = (pet.x as i32 - vw / 2).clamp(-vw, screen_w);
                        let win_y = (pet.y as i32 + PET_H as i32 - vh).clamp(-vh, screen_h - vh);

                        if vid_w as u16 != cur_win_w || vid_h as u16 != cur_win_h {
                            conn.free_pixmap(backing)?;
                            backing = conn.generate_id()?;
                            conn.create_pixmap(depth, backing, win, vid_w as u16, vid_h as u16)?;
                            conn.configure_window(win, &ConfigureWindowAux::new()
                                .width(vid_w).height(vid_h))?;
                            cur_win_w = vid_w as u16;
                            cur_win_h = vid_h as u16;
                            last_input_shape = None;
                        }

                        conn.configure_window(win, &ConfigureWindowAux::new().x(win_x).y(win_y))?;
                        put_rgba_image(&conn, backing, gc, rgba, depth, vid_w as u16, vid_h as u16)?;
                        conn.copy_area(backing, win, gc, 0, 0, 0, 0, vid_w as u16, vid_h as u16)?;
                        if last_input_shape.is_none() {
                            set_input_shape_rect(&conn, win, 0, 0, vid_w as u16, vid_h as u16)?;
                            last_input_shape = Some((0, 0, vid_w as u16, vid_h as u16));
                        }
                        conn.flush()?;
                    }
                }
            }
            let elapsed = tick_start.elapsed();
            if elapsed < Duration::from_millis(TICK_MS) {
                thread::sleep(Duration::from_millis(TICK_MS) - elapsed);
            }
            continue;
        }

        let max_scale = (WIN_SIZE_MAX as f64 / PET_W as f64) * 0.99;
        pet.scale = current_config.base_scale.min(max_scale);
        pet.frame += 1;

        let (frame_data_owned, ticks_per_frame_used): (Option<Vec<u8>>, u32) =
            if matches!(behavior.behavior_type, shared::BehaviorType::Sequence) {
                let steps = &behavior.steps;
                if !steps.is_empty() {
                    let step = &steps[pet.seq_step.min(steps.len() - 1)];
                    if !step.frames.is_empty() {
                        let fi = (pet.frame as usize / step.ticks_per_frame as usize) % step.frames.len();
                        (std::fs::read(&step.frames[fi]).ok(), step.ticks_per_frame)
                    } else { (None, 6) }
                } else { (None, 6) }
            } else { (None, 6) };

        let loaded_anim = anim_id_for_config(&characters, &current_config)
            .expect("anim_id_for_config returned None in frame rendering path (bug: animation should be frame type)");

        let frame_data: &[u8] = if let Some(ref owned) = frame_data_owned {
            owned.as_slice()
        } else {
            let tpf = loaded_anim.ticks_per_frame;
            let fi = (pet.frame as usize / tpf as usize) % loaded_anim.frames.len();
            &loaded_anim.frames[fi]
        };

        let render_w = ((PET_W as f64 * pet.scale) as u32).max(1).min(WIN_SIZE_MAX as u32);
        let render_h = ((PET_H as f64 * pet.scale) as u32).max(1).min(WIN_SIZE_MAX as u32);

        let size_changed = render_w as u16 != cur_win_w || render_h as u16 != cur_win_h;
        if size_changed {
            conn.free_pixmap(backing)?;
            backing = conn.generate_id()?;
            conn.create_pixmap(depth, backing, win, render_w as u16, render_h as u16)?;
            conn.configure_window(win, &ConfigureWindowAux::new()
                .width(render_w).height(render_h))?;
            cur_win_w = render_w as u16;
            cur_win_h = render_h as u16;
            last_input_shape = None;
        }

        let win_x = (pet.x as i32 - render_w as i32 / 2).clamp(-(render_w as i32), screen_w);
        let win_y = (pet.y as i32 + PET_H as i32 - render_h as i32).clamp(-(render_h as i32), screen_h - render_h as i32);

        conn.configure_window(win, &ConfigureWindowAux::new()
            .x(win_x)
            .y(win_y))?;

        let rendered = render_svg(frame_data, render_w, render_h);
        put_rgba_image(&conn, backing, gc, &rendered, depth, render_w as u16, render_h as u16)?;
        conn.copy_area(backing, win, gc, 0, 0, 0, 0, render_w as u16, render_h as u16)?;

        let new_shape = (0i16, 0i16, render_w as u16, render_h as u16);
        if last_input_shape != Some(new_shape) {
            set_input_shape_rect(&conn, win, 0, 0, render_w as u16, render_h as u16)?;
            last_input_shape = Some(new_shape);
        }

        conn.flush()?;

        let elapsed = tick_start.elapsed();
        if elapsed < Duration::from_millis(TICK_MS) {
            thread::sleep(Duration::from_millis(TICK_MS) - elapsed);
        }
    }
}

fn find_argb_visual(conn: &RustConnection, screen_num: usize) -> Option<(u8, u32)> {
    let screen = &conn.setup().roots[screen_num];
    for depth_info in &screen.allowed_depths {
        if depth_info.depth == 32 {
            for visual in &depth_info.visuals {
                if visual.class == VisualClass::TRUE_COLOR {
                    return Some((32, visual.visual_id));
                }
            }
        }
    }
    None
}

fn set_empty_input_shape(conn: &RustConnection, win: Window) -> Result<(), Box<dyn std::error::Error>> {
    let pixmap = conn.generate_id()?;
    conn.create_pixmap(1, pixmap, win, 1, 1)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0))?;
    conn.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }])?;
    conn.shape_mask(shape::SO::SET, SK::INPUT, win, 0, 0, pixmap)?;
    conn.free_gc(gc)?;
    conn.free_pixmap(pixmap)?;
    Ok(())
}

fn set_input_shape_rect(conn: &RustConnection, win: Window, x: i16, y: i16, w: u16, h: u16) -> Result<(), Box<dyn std::error::Error>> {
    conn.shape_rectangles(
        shape::SO::SET, SK::INPUT, x11rb::protocol::xproto::ClipOrdering::UNSORTED,
        win, 0, 0, &[Rectangle { x, y, width: w, height: h }],
    )?;
    Ok(())
}

fn scale_pixels(src: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let fx = (dx as f64 * (src_w - 1) as f64) / (dst_w - 1).max(1) as f64;
            let fy = (dy as f64 * (src_h - 1) as f64) / (dst_h - 1).max(1) as f64;
            let x0 = fx.floor() as usize;
            let y0 = fy.floor() as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let y1 = (y0 + 1).min(src_h - 1);
            let wx = fx - x0 as f64;
            let wy = fy - y0 as f64;

            let i00 = (y0 * src_w + x0) * 4;
            let i10 = (y0 * src_w + x1) * 4;
            let i01 = (y1 * src_w + x0) * 4;
            let i11 = (y1 * src_w + x1) * 4;
            let di = (dy * dst_w + dx) * 4;

            for c in 0..4 {
                if i11 + c < src.len() && di + c < dst.len() {
                    let v00 = src[i00 + c] as f64;
                    let v10 = src[i10 + c] as f64;
                    let v01 = src[i01 + c] as f64;
                    let v11 = src[i11 + c] as f64;
                    let v = v00 * (1.0 - wx) * (1.0 - wy)
                          + v10 * wx * (1.0 - wy)
                          + v01 * (1.0 - wx) * wy
                          + v11 * wx * wy;
                    dst[di + c] = v as u8;
                }
            }
        }
    }
    dst
}

fn put_rgba_image(
    conn: &RustConnection,
    win: Window,
    gc: Gcontext,
    rgba: &[u8],
    depth: u8,
    w: u16,
    h: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let pixel_count = w as usize * h as usize;
    let mut bgra = vec![0u8; pixel_count * 4];
    for i in 0..pixel_count {
        let src = i * 4;
        if src + 3 < rgba.len() {
            let r = rgba[src];
            let g = rgba[src + 1];
            let b = rgba[src + 2];
            let a = rgba[src + 3];
            bgra[i * 4] = b;
            bgra[i * 4 + 1] = g;
            bgra[i * 4 + 2] = r;
            bgra[i * 4 + 3] = a;
        }
    }

    conn.put_image(
        ImageFormat::Z_PIXMAP,
        win,
        gc,
        w,
        h,
        0,
        0,
        0,
        depth,
        &bgra,
    )?;
    Ok(())
}

fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = vec![0u8; rgba.len()];
    for i in 0..rgba.len() / 4 {
        bgra[i*4]   = rgba[i*4+2];
        bgra[i*4+1] = rgba[i*4+1];
        bgra[i*4+2] = rgba[i*4];
        bgra[i*4+3] = rgba[i*4+3];
    }
    bgra
}



fn put_rgba_image_at(
    conn: &RustConnection,
    win: Window,
    gc: Gcontext,
    rgba: &[u8],
    depth: u8,
    dst_x: u16,
    dst_y: u16,
    w: u16,
    h: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let row_bytes = w as usize * 4;
    let chunk_rows: u16 = (65536 / row_bytes.max(1)).max(1).min(h as usize) as u16;
    let mut y_off: u16 = 0;
    while y_off < h {
        let rows = chunk_rows.min(h - y_off);
        let src_start = y_off as usize * row_bytes;
        let src_end = src_start + rows as usize * row_bytes;
        let src_slice = &rgba[src_start..src_end.min(rgba.len())];
        let pixel_count = rows as usize * w as usize;
        let mut bgra = vec![0u8; pixel_count * 4];
        for i in 0..pixel_count {
            let src = i * 4;
            if src + 3 < src_slice.len() {
                bgra[i * 4]     = src_slice[src + 2];
                bgra[i * 4 + 1] = src_slice[src + 1];
                bgra[i * 4 + 2] = src_slice[src];
                bgra[i * 4 + 3] = src_slice[src + 3];
            }
        }
        conn.put_image(ImageFormat::Z_PIXMAP, win, gc, w, rows, dst_x as i16, (dst_y + y_off) as i16, 0, depth, &bgra)?;
        y_off += rows;
    }
    Ok(())
}

fn load_png_rgba(data: &[u8]) -> Vec<u8> {
    let mut decoder = png::Decoder::new(data);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().expect("Failed to read PNG");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("Failed to decode PNG");
    buf.truncate(info.buffer_size());

    let expected = (PET_W as usize) * (PET_H as usize) * 4;
    if buf.len() < expected {
        buf.resize(expected, 0);
    }
    buf
}

fn find_surface_below(x: f64, y: f64, surfaces: &[Surface]) -> Option<Surface> {
    let mut best: Option<(i32, Surface)> = None;
    for s in surfaces {
        if x + PET_W as f64 > s.x as f64 && x < (s.x + s.w) as f64 && s.y as f64 > y + PET_H as f64 {
            match &best {
                None => best = Some((s.y, s.clone())),
                Some((by, _)) => {
                    if s.y < *by {
                        best = Some((s.y, s.clone()));
                    }
                }
            }
        }
    }
    best.map(|(_, s)| s)
}

fn find_surface_at(x: f64, y: f64, surfaces: &[Surface]) -> Option<Surface> {
    for s in surfaces {
        let sx = s.x as f64;
        let sy = s.y as f64;
        let sw = s.w as f64;
        if x >= sx && x <= sx + sw && (y - sy).abs() < 8.0 {
            return Some(s.clone());
        }
    }
    None
}

fn get_net_client_list(conn: &RustConnection, root: Window) -> Option<Vec<Window>> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    for prop_name in [b"_NET_CLIENT_LIST_STACKING".as_ref(), b"_NET_CLIENT_LIST".as_ref()] {
        let atom = match conn.intern_atom(false, prop_name).ok().and_then(|c| c.reply().ok()) {
            Some(r) if r.atom != 0 => r.atom,
            _ => continue,
        };
        let reply = match conn.get_property(false, root, atom, x11rb::protocol::xproto::AtomEnum::WINDOW, 0, 2048)
            .ok().and_then(|c| c.reply().ok()) {
            Some(r) if !r.value.is_empty() => r,
            _ => continue,
        };
        if let Some(wins) = reply.value32().map(|i| i.collect::<Vec<_>>()) {
            if !wins.is_empty() {
                return Some(wins);
            }
        }
    }
    None
}

fn scan_surfaces(conn: &RustConnection, root: Window, our_win: Window) -> Vec<Surface> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let windows = get_net_client_list(conn, root).unwrap_or_else(|| {
        conn.query_tree(root).ok()
            .and_then(|c| c.reply().ok())
            .map(|t| t.children)
            .unwrap_or_default()
    });

    let mut surfaces = Vec::new();
    for child in &windows {
        if *child == our_win { continue; }

        let attrs = match conn.get_window_attributes(*child).ok().and_then(|c| c.reply().ok()) {
            Some(a) => a,
            None => continue,
        };
        if attrs.map_state != x11rb::protocol::xproto::MapState::VIEWABLE { continue; }

        let geom = match conn.get_geometry(*child).ok().and_then(|c| c.reply().ok()) {
            Some(g) => g,
            None => continue,
        };
        if geom.width < 100 || geom.height < 50 { continue; }

        let trans = match conn.translate_coordinates(*child, root, 0, 0).ok().and_then(|c| c.reply().ok()) {
            Some(t) => t,
            None => continue,
        };
        let x = trans.dst_x as i32;
        let y = trans.dst_y as i32;
        let w = geom.width as i32;
        let h = geom.height as i32;

        if y < 0 || x + w <= 0 { continue; }

        tux_log!("[pet] surface: x={} y={} w={} h={}", x, y, w, h);
        surfaces.push(Surface { x, y, w });
    }
    surfaces
}

#[allow(dead_code)]
fn poll_surfaces() -> Result<Vec<Surface>, ()> {
    let script = r#"
import gi, json
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi
Atspi.init()
desktop = Atspi.get_desktop(0)
windows = []
for i in range(desktop.get_child_count()):
    app = desktop.get_child_at_index(i)
    if app is None:
        continue
    app_name = app.get_name() or ""
    if "gnome-shell" in app_name.lower():
        continue
    for j in range(app.get_child_count()):
        win = app.get_child_at_index(j)
        if win is None:
            continue
        role = win.get_role_name()
        if role not in ("frame", "window"):
            continue
        title = win.get_name() or ""
        if not title:
            continue
        try:
            ext = win.get_extents(Atspi.CoordType.SCREEN)
            if ext.width > 50 and ext.height > 50:
                windows.append({"x": ext.x, "y": ext.y, "w": ext.width})
        except:
            pass
print(json.dumps(windows))
"#;

    let output = Command::new("timeout")
        .arg("3")
        .arg("/usr/bin/python3")
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|_| ())?;

    if !output.status.success() {
        return Err(());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|_| ())?;

    let mut rects = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for item in arr {
            let x = item.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let y = item.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let w = item.get("w").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            if w > 50 {
                rects.push(Surface { x, y, w });
            }
        }
    }
    Ok(rects)
}

fn get_workarea(conn: &RustConnection, root: Window) -> Option<(i32, i32, i32, i32)> {
    let atom = conn.intern_atom(false, b"_NET_WORKAREA").ok()?.reply().ok()?.atom;
    let reply = conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, 4)
        .ok()?.reply().ok()?;
    let values = reply.value32()?.collect::<Vec<u32>>();
    if values.len() >= 4 {
        Some((values[0] as i32, values[1] as i32, values[2] as i32, values[3] as i32))
    } else {
        None
    }
}
