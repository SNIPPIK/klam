//! Графический интерфейс Klam.
//!
//! GUI всегда работает от обычного пользователя. Ядро (`klam server` /
//! `klam client`) запускается отдельным процессом:
//!   * Linux: через `pkexec` (системный запрос пароля при каждом запуске);
//!     если GUI уже запущен от root — напрямую;
//!   * Windows: напрямую.
//!
//! Остановка: GUI закрывает stdin дочернего процесса, ядро (флаг
//! `--exit-on-stdin-eof`) видит EOF и завершается. То же происходит, если
//! GUI закрыли или он упал.

use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align, Align2, Button, Color32, FontId, Layout, Rect, RichText, Rounding, Sense, Stroke,
    TextEdit, vec2,
};

use crate::{
    discovery::{self, FoundServers},
    protocol::Direction,
};

const MAX_LOG_LINES: usize = 500;
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const SERVER_TTL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Палитра
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(17, 19, 26);
const CARD: Color32 = Color32::from_rgb(26, 29, 39);
const ROW: Color32 = Color32::from_rgb(34, 38, 51);
const HOVER: Color32 = Color32::from_rgb(44, 49, 66);
const BORDER: Color32 = Color32::from_rgb(48, 53, 70);
const LOG_BG: Color32 = Color32::from_rgb(12, 14, 19);
const TEXT: Color32 = Color32::from_rgb(232, 234, 240);
const MUTED: Color32 = Color32::from_rgb(140, 147, 166);
const ACCENT: Color32 = Color32::from_rgb(99, 102, 241);
const GREEN: Color32 = Color32::from_rgb(52, 199, 123);
const AMBER: Color32 = Color32::from_rgb(245, 176, 65);
const RED: Color32 = Color32::from_rgb(235, 87, 87);

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Klam")
            .with_inner_size([500.0, 680.0])
            .with_min_inner_size([440.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Klam",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
        .map_err(|error| anyhow::anyhow!("GUI error: {error}"))
}

fn setup_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = LOG_BG;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    let rounding = Rounding::same(8.0);

    visuals.widgets.noninteractive.rounding = rounding;
    visuals.widgets.inactive.rounding = rounding;
    visuals.widgets.hovered.rounding = rounding;
    visuals.widgets.active.rounding = rounding;
    visuals.widgets.open.rounding = rounding;

    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.bg_fill = ROW;
    visuals.widgets.inactive.weak_bg_fill = ROW;
    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.weak_bg_fill = HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);

    ctx.set_visuals(visuals);

    ctx.style_mut(|style| {
        style.spacing.item_spacing = vec2(10.0, 10.0);
        style.spacing.button_padding = vec2(14.0, 8.0);
        style.spacing.interact_size.y = 32.0;
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Client,
    Server,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Busy,
    Active,
    Error,
}

struct Runner {
    child: Child,
    stdin: Option<ChildStdin>,
    stopping_since: Option<Instant>,
}

struct App {
    mode: Mode,
    found: FoundServers,
    address: String,
    side: Direction,
    width: String,
    height: String,
    runner: Option<Runner>,
    log: Arc<Mutex<Vec<String>>>,
    state: State,
    status: String,
}

impl App {
    fn new() -> Self {
        Self {
            mode: Mode::Client,
            found: discovery::spawn_listener(),
            address: String::new(),
            side: Direction::Left,
            width: String::new(),
            height: String::new(),
            runner: None,
            log: Arc::new(Mutex::new(Vec::new())),
            state: State::Idle,
            status: "Не запущено".to_string(),
        }
    }

    fn set_status(&mut self, state: State, text: impl Into<String>) {
        self.state = state;
        self.status = text.into();
    }

    fn servers(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = self
            .found
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, server)| server.last_seen.elapsed() < SERVER_TTL)
            .map(|((ip, port), server)| (server.name.clone(), format!("{ip}:{port}")))
            .collect();

        list.sort();
        list
    }

    fn poll_runner(&mut self) {
        let Some(runner) = self.runner.as_mut() else {
            return;
        };

        if let Some(since) = runner.stopping_since {
            if since.elapsed() > STOP_TIMEOUT {
                let _ = runner.child.kill();
            }
        }

        if let Ok(Some(status)) = runner.child.try_wait() {
            match status.code() {
                Some(0) => self.set_status(State::Idle, "Остановлено"),

                Some(126) | Some(127) => self.set_status(
                    State::Error,
                    "Запуск отменён или нет прав (запрос пароля отклонён)",
                ),

                Some(code) => self.set_status(State::Error, format!("Завершилось с кодом {code}")),
                None => self.set_status(State::Error, "Завершено сигналом"),
            }

            self.runner = None;
        }
    }

    fn start(&mut self, args: Vec<String>, status: &str) {
        self.log.lock().unwrap().clear();

        match spawn_core(&args, &self.log) {
            Ok(runner) => {
                self.runner = Some(runner);
                self.set_status(State::Active, status);
            }

            Err(error) => {
                self.set_status(State::Error, format!("Не удалось запустить: {error:#}"));
            }
        }
    }

    fn stop(&mut self) {
        if let Some(runner) = self.runner.as_mut() {
            runner.stdin.take();
            runner.stopping_since = Some(Instant::now());
            self.set_status(State::Busy, "Останавливаю...");
        }
    }

    fn size_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        if let Some(width) = positive(&self.width) {
            args.push("--screen-width".to_string());
            args.push(width.to_string());
        }

        if let Some(height) = positive(&self.height) {
            args.push("--screen-height".to_string());
            args.push(height.to_string());
        }

        args
    }

    fn start_client(&mut self) {
        let address = self.address.trim().to_string();

        if address.is_empty() {
            self.set_status(State::Error, "Выберите сервер или введите адрес");
            return;
        }

        let mut args = vec![
            "client".to_string(),
            "--connect".to_string(),
            address,
            "--direction".to_string(),
            direction_arg(self.side).to_string(),
        ];

        args.extend(self.size_args());
        args.push("--exit-on-stdin-eof".to_string());

        self.start(args, "Подключение...");
    }

    fn start_server(&mut self) {
        // На Linux размер экрана обязателен, на Windows определяется сам.
        if !cfg!(windows) && (positive(&self.width).is_none() || positive(&self.height).is_none())
        {
            self.set_status(State::Error, "Укажите ширину и высоту экрана");
            return;
        }

        let mut args = vec!["server".to_string()];
        args.extend(self.size_args());
        args.push("--exit-on-stdin-eof".to_string());

        self.start(args, "Сервер запущен");
    }

    // -----------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());

            ui.painter().rect(rect, 12.0, ACCENT, Stroke::NONE);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "K",
                FontId::proportional(24.0),
                Color32::WHITE,
            );

            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Klam").size(22.0).strong());
                ui.label(RichText::new("Одна мышь и клавиатура на всех").size(12.5).color(MUTED));
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                status_pill(ui, self.state, &self.status);
            });
        });
    }

    fn mode_tabs(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.horizontal(|ui| {
            for (mode, label) in [(Mode::Client, "Клиент"), (Mode::Server, "Сервер")] {
                let selected = self.mode == mode;

                let button = Button::new(
                    RichText::new(label)
                        .size(15.0)
                        .color(if selected { Color32::WHITE } else { MUTED }),
                )
                    .fill(if selected { ACCENT } else { ROW })
                    .rounding(8.0)
                    .min_size(vec2(110.0, 34.0));

                if ui.add_enabled(!running, button).clicked() {
                    self.mode = mode;
                }
            }
        });
    }

    fn client_ui(&mut self, ui: &mut egui::Ui, running: bool) {
        let servers = self.servers();

        ui.add_enabled_ui(!running, |ui| {
            card(ui, |ui| {
                caption(ui, "СЕРВЕР");

                if servers.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Ищу серверы в сети...")
                                .color(MUTED),
                        );
                    });
                }

                for (name, address) in &servers {
                    if server_row(ui, name, address, self.address == *address, !running) {
                        self.address = address.clone();
                    }
                }

                ui.add(
                    TextEdit::singleline(&mut self.address)
                        .hint_text("или введите адрес: 192.168.1.10:24801")
                        .desired_width(f32::INFINITY),
                );
            });

            card(ui, |ui| {
                caption(ui, "ГДЕ ЭТОТ КОМПЬЮТЕР ОТНОСИТЕЛЬНО СЕРВЕРА");
                side_picker(ui, &mut self.side, !running);
            });

            card(ui, |ui| {
                caption(ui, "ЭКРАН (НЕОБЯЗАТЕЛЬНО)");
                self.size_fields(ui);
            });
        });

        if running {
            if big_button(ui, "Остановить", RED) {
                self.stop();
            }
        } else if big_button(ui, "Подключиться", ACCENT) {
            self.start_client();
        }
    }

    fn server_ui(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.add_enabled_ui(!running, |ui| {
            card(ui, |ui| {
                caption(ui, "ЭТОТ КОМПЬЮТЕР — СЕРВЕР");
                ui.label(
                    RichText::new("Клиенты найдут его автоматически (UDP 24802).")
                        .color(MUTED),
                );
            });

            card(ui, |ui| {
                caption(ui, "ЭКРАН");
                self.size_fields(ui);

                if cfg!(windows) {
                    ui.label(
                        RichText::new("Пусто — размер определится автоматически.")
                            .size(12.5)
                            .color(MUTED),
                    );
                }
            });
        });

        if running {
            if big_button(ui, "Остановить", RED) {
                self.stop();
            }
        } else if big_button(ui, "Запустить сервер", ACCENT) {
            self.start_server();
        }
    }

    fn size_fields(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.width)
                    .desired_width(80.0)
                    .hint_text("ширина"),
            );

            ui.label(RichText::new("×").color(MUTED));

            ui.add(
                TextEdit::singleline(&mut self.height)
                    .desired_width(80.0)
                    .hint_text("высота"),
            );

            ui.label(RichText::new("px").color(MUTED));
        });
    }

    fn log_ui(&self, ui: &mut egui::Ui) {
        let lines: Vec<String> = self.log.lock().unwrap().clone();

        egui::CollapsingHeader::new(RichText::new("Журнал").color(MUTED))
            .default_open(false)
            .show(ui, |ui| {
                egui::Frame::none()
                    .fill(LOG_BG)
                    .rounding(8.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());

                                if lines.is_empty() {
                                    ui.label(RichText::new("пусто").size(12.0).color(MUTED));
                                }

                                for line in &lines {
                                    ui.label(
                                        RichText::new(line).monospace().size(11.5).color(MUTED),
                                    );
                                }
                            });
                    });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_runner();

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(18.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let running = self.runner.is_some();

                        self.header(ui);
                        ui.add_space(4.0);
                        self.mode_tabs(ui, running);

                        match self.mode {
                            Mode::Client => self.client_ui(ui, running),
                            Mode::Server => self.server_ui(ui, running),
                        }

                        self.log_ui(ui);
                    });
            });

        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// Виджеты
// ---------------------------------------------------------------------------

fn card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(CARD)
        .rounding(12.0)
        .stroke(Stroke::new(1.0, BORDER))
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        });
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).strong().color(MUTED));
}

fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32) -> bool {
    ui.add(
        Button::new(RichText::new(text).size(16.0).strong().color(Color32::WHITE))
            .fill(fill)
            .rounding(10.0)
            .min_size(vec2(ui.available_width(), 44.0)),
    )
        .clicked()
}

fn status_pill(ui: &mut egui::Ui, state: State, text: &str) {
    let color = match state {
        State::Idle => MUTED,
        State::Busy => AMBER,
        State::Active => GREEN,
        State::Error => RED,
    };

    ui.label(RichText::new(text).size(12.5).color(color));

    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

/// Строка найденного сервера. `true`, если по ней кликнули.
fn server_row(ui: &mut egui::Ui, name: &str, address: &str, selected: bool, enabled: bool) -> bool {
    let size = vec2(ui.available_width(), 48.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    let fill = if selected {
        ACCENT.gamma_multiply(0.25)
    } else if response.hovered() && enabled {
        HOVER
    } else {
        ROW
    };

    let stroke = if selected {
        Stroke::new(1.5, ACCENT)
    } else {
        Stroke::new(1.0, BORDER)
    };

    let painter = ui.painter();

    painter.rect(rect, 10.0, fill, stroke);
    painter.circle_filled(rect.left_center() + vec2(18.0, 0.0), 5.0, GREEN);
    painter.text(
        rect.left_top() + vec2(36.0, 8.0),
        Align2::LEFT_TOP,
        name,
        FontId::proportional(15.0),
        TEXT,
    );
    painter.text(
        rect.left_top() + vec2(36.0, 28.0),
        Align2::LEFT_TOP,
        address,
        FontId::monospace(12.0),
        MUTED,
    );

    enabled && response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Схема: сервер в центре, по краям — четыре стороны, где может стоять клиент.
fn side_picker(ui: &mut egui::Ui, side: &mut Direction, enabled: bool) {
    let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), 180.0), Sense::hover());
    let painter = ui.painter_at(area);
    let center = area.center();

    let server = Rect::from_center_size(center, vec2(110.0, 56.0));

    painter.rect(
        server,
        10.0,
        ROW,
        Stroke::new(1.5, MUTED),
    );
    painter.text(
        server.center(),
        Align2::CENTER_CENTER,
        "Сервер",
        FontId::proportional(15.0),
        TEXT,
    );

    let zones = [
        (
            Direction::Top,
            "Сверху",
            Rect::from_center_size(center + vec2(0.0, -54.0), vec2(110.0, 36.0)),
        ),
        (
            Direction::Bottom,
            "Снизу",
            Rect::from_center_size(center + vec2(0.0, 54.0), vec2(110.0, 36.0)),
        ),
        (
            Direction::Left,
            "Слева",
            Rect::from_center_size(center + vec2(-108.0, 0.0), vec2(90.0, 56.0)),
        ),
        (
            Direction::Right,
            "Справа",
            Rect::from_center_size(center + vec2(108.0, 0.0), vec2(90.0, 56.0)),
        ),
    ];

    for (direction, label, rect) in zones {
        let response = ui.interact(rect, ui.id().with(label), Sense::click());
        let selected = *side == direction;

        let fill = if selected {
            ACCENT
        } else if response.hovered() && enabled {
            HOVER
        } else {
            CARD
        };

        let stroke = if selected {
            Stroke::new(1.5, ACCENT)
        } else {
            Stroke::new(1.0, BORDER)
        };

        painter.rect(rect, 10.0, fill, stroke);

        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            if selected { "Этот ПК" } else { label },
            FontId::proportional(14.0),
            if selected { Color32::WHITE } else { MUTED },
        );

        if enabled && response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *side = direction;
        }
    }
}

fn direction_arg(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Top => "top",
        Direction::Bottom => "bottom",
    }
}

fn positive(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok().filter(|value| *value > 0)
}

// ---------------------------------------------------------------------------
// Запуск ядра
// ---------------------------------------------------------------------------

fn spawn_core(args: &[String], log: &Arc<Mutex<Vec<String>>>) -> Result<Runner> {
    let exe = std::env::current_exe().context("cannot locate klam executable")?;

    let mut command = build_command(&exe, args);

    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().context("failed to start klam core")?;

    let stdin = child.stdin.take();

    if let Some(stdout) = child.stdout.take() {
        pipe_lines(stdout, log.clone());
    }

    if let Some(stderr) = child.stderr.take() {
        pipe_lines(stderr, log.clone());
    }

    Ok(Runner {
        child,
        stdin,
        stopping_since: None,
    })
}

fn pipe_lines<R>(reader: R, log: Arc<Mutex<Vec<String>>>)
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(|line| line.ok()) {
            let mut log = log.lock().unwrap();

            log.push(line);

            if log.len() > MAX_LOG_LINES {
                let extra = log.len() - MAX_LOG_LINES;
                log.drain(..extra);
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn current_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().ok()?;

    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn build_command(exe: &Path, args: &[String]) -> Command {
    let uid = current_uid();

    // Уже root — pkexec не нужен.
    if uid == Some(0) {
        let mut command = Command::new(exe);
        command.args(args);
        return command;
    }

    // pkexec сбрасывает окружение, а буферу обмена нужна графическая
    // сессия пользователя: передаём её через те же переменные, которые
    // читает `ClipboardSession::detect` (SUDO_USER, SUDO_UID, ...).
    let mut command = Command::new("pkexec");

    command.arg("env");

    if let Ok(user) = std::env::var("USER") {
        command.arg(format!("SUDO_USER={user}"));
    }

    if let Some(uid) = uid {
        command.arg(format!("SUDO_UID={uid}"));
    }

    for key in ["XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY"] {
        if let Ok(value) = std::env::var(key) {
            command.arg(format!("{key}={value}"));
        }
    }

    command.arg(exe);
    command.args(args);
    command
}

#[cfg(windows)]
fn build_command(exe: &Path, args: &[String]) -> Command {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = Command::new(exe);
    command.args(args);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}