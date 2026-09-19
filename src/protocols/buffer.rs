use std::{
    io::Write,
    path::Path,
    process::{
        Command,
        Stdio,
    },
};

use anyhow::{
    bail,
    Context,
    Result,
};

use crate::protocol::WireEvent;

pub const DEFAULT_HANDOFF_CAPACITY: usize =
    128;

/*
 * ========================================================================
 * HANDOFF BUFFER
 * ========================================================================
 */

#[derive(Debug, Default)]
pub struct HandoffBuffer {
    events: Vec<WireEvent>,
}

impl HandoffBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(
                capacity.max(1),
            ),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    #[inline]
    pub fn push(
        &mut self,
        event: WireEvent,
    ) {
        self.events.push(event);
    }

    #[inline]
    pub fn extend(
        &mut self,
        events: &[WireEvent],
    ) {
        self.events
            .extend_from_slice(events);
    }

    #[inline]
    pub fn clear(&mut self) {
        self.events.clear();
    }

    #[inline]
    pub fn take(&mut self) -> Vec<WireEvent> {
        std::mem::take(
            &mut self.events,
        )
    }
}

/*
 * ========================================================================
 * CLIPBOARD
 * ========================================================================
 */

#[derive(Debug, Clone, Copy)]
enum ClipboardBackend {
    Wayland,
    X11Xclip,
    X11Xsel,
}

#[derive(Debug, Clone)]
struct ClipboardSession {
    user: Option<String>,
    uid: Option<String>,

    runtime_dir: Option<String>,

    wayland_display: Option<String>,

    display: Option<String>,
    xauthority: Option<String>,
}

impl ClipboardSession {
    fn detect() -> Result<Self> {
        let user =
            std::env::var("SUDO_USER")
                .ok()
                .or_else(|| {
                    std::env::var("USER")
                        .ok()
                });

        let uid =
            std::env::var("SUDO_UID")
                .ok();

        /*
         * Для sudo:
         *
         *   /run/user/<uid>
         */
        let runtime_dir =
            std::env::var(
                "XDG_RUNTIME_DIR",
            )
                .ok()
                .or_else(|| {
                    uid.as_deref()
                        .map(|uid| {
                            format!(
                                "/run/user/{}",
                                uid
                            )
                        })
                });

        /*
         * WAYLAND_DISPLAY может отсутствовать
         * в root environment после sudo.
         *
         * В таком случае ищем сокет напрямую.
         */
        let wayland_display =
            std::env::var(
                "WAYLAND_DISPLAY",
            )
                .ok()
                .or_else(|| {
                    runtime_dir
                        .as_deref()
                        .and_then(
                            find_wayland_display,
                        )
                });

        let display =
            std::env::var(
                "DISPLAY",
            )
                .ok();

        let xauthority =
            std::env::var(
                "XAUTHORITY",
            )
                .ok()
                .or_else(|| {
                    user.as_deref()
                        .map(|user| {
                            format!(
                                "/home/{}/.Xauthority",
                                user
                            )
                        })
                        .filter(|path| {
                            Path::new(path)
                                .exists()
                        })
                });

        Ok(Self {
            user,
            uid,
            runtime_dir,
            wayland_display,
            display,
            xauthority,
        })
    }
}

#[derive(Debug)]
pub struct ClipboardBuffer {
    backend: ClipboardBackend,
    session: ClipboardSession,
    last_hash: u64,
}

impl ClipboardBuffer {
    pub fn new() -> Result<Self> {
        let session =
            ClipboardSession::detect()?;

        let backend =
            detect_backend(
                &session,
            )?;

        /*
         * Начальный clipboard считаем уже синхронизированным.
         *
         * При старте klam содержимое clipboard автоматически
         * не отправляется в сеть.
         */
        let initial_text =
            read_clipboard(
                backend,
                &session,
            )
                .unwrap_or_default();

        let last_hash =
            hash_text(
                &initial_text,
            );

        eprintln!(
            "[CLIPBOARD] \
             backend={:?} \
             user={:?} \
             uid={:?} \
             runtime={:?} \
             wayland={:?} \
             display={:?}",
            backend,
            session.user,
            session.uid,
            session.runtime_dir,
            session.wayland_display,
            session.display,
        );

        Ok(Self {
            backend,
            session,
            last_hash,
        })
    }

    /*
     * Возвращает новое локальное значение clipboard.
     */
    pub fn poll_changed(
        &mut self,
    ) -> Result<Option<(u64, String)>> {
        let text =
            match read_clipboard(
                self.backend,
                &self.session,
            ) {
                Ok(text) => text,

                Err(error) => {
                    eprintln!(
                        "[CLIPBOARD] read error: {}",
                        error
                    );

                    return Ok(None);
                }
            };

        let hash =
            hash_text(
                &text,
            );

        if hash ==
            self.last_hash
        {
            return Ok(None);
        }

        self.last_hash =
            hash;

        Ok(Some((
            hash,
            text,
        )))
    }

    /*
     * Устанавливает clipboard, полученный из сети.
     *
     * last_hash обновляется сразу, поэтому следующий
     * poll не отправит полученное значение обратно.
     */
    pub fn set_remote_text(
        &mut self,
        hash: u64,
        text: &str,
    ) -> Result<()> {
        write_clipboard(
            self.backend,
            &self.session,
            text,
        )?;

        self.last_hash =
            hash;

        Ok(())
    }

    #[inline]
    pub fn current_hash(
        &self,
    ) -> u64 {
        self.last_hash
    }
}

/*
 * ========================================================================
 * BACKEND DETECTION
 * ========================================================================
 */

fn detect_backend(
    session: &ClipboardSession,
) -> Result<ClipboardBackend> {
    /*
     * ------------------------------------------------------------
     * WAYLAND
     * ------------------------------------------------------------
     */
    if session.wayland_display
        .is_some()
    {
        let has_wl_paste =
            command_exists(
                session,
                "wl-paste",
            );

        let has_wl_copy =
            command_exists(
                session,
                "wl-copy",
            );

        if has_wl_paste &&
            has_wl_copy
        {
            return Ok(
                ClipboardBackend::Wayland,
            );
        }
    }

    /*
     * ------------------------------------------------------------
     * X11 / xclip
     * ------------------------------------------------------------
     */
    if session.display
        .is_some()
        && command_exists(
        session,
        "xclip",
    )
    {
        return Ok(
            ClipboardBackend::X11Xclip,
        );
    }

    /*
     * ------------------------------------------------------------
     * X11 / xsel
     * ------------------------------------------------------------
     */
    if session.display
        .is_some()
        && command_exists(
        session,
        "xsel",
    )
    {
        return Ok(
            ClipboardBackend::X11Xsel,
        );
    }

    bail!(
        "no supported clipboard backend found; \
         install wl-clipboard for Wayland \
         or xclip/xsel for X11"
    );
}

/*
 * ========================================================================
 * SESSION COMMAND
 * ========================================================================
 */

fn command_exists(
    session: &ClipboardSession,
    command: &str,
) -> bool {
    match run_session_command(
        session,
        "sh",
        &[
            "-c",
            &format!(
                "command -v {} >/dev/null 2>&1",
                command
            ),
        ],
    ) {
        Ok(output) =>
            output.status.success(),

        Err(_) =>
            false,
    }
}

fn run_session_command(
    session: &ClipboardSession,
    command: &str,
    args: &[&str],
) -> Result<std::process::Output> {
    let user =
        session
            .user
            .as_deref();

    /*
     * ------------------------------------------------------------
     * Обычный запуск без sudo.
     * ------------------------------------------------------------
     */
    if user.is_none() {
        let mut cmd =
            Command::new(
                command,
            );

        apply_session_env(
            &mut cmd,
            session,
        );

        return cmd
            .args(args)
            .output()
            .context(
                "failed to execute clipboard command",
            );
    }

    /*
     * ------------------------------------------------------------
     * Запуск от пользователя графической сессии.
     * ------------------------------------------------------------
     *
     * Не используем libc.
     *
     * Если klam запущен через sudo, SUDO_USER указывает
     * пользователя, чей clipboard нам нужен.
     */
    let mut cmd =
        Command::new(
            "runuser",
        );

    cmd.args([
        "-u",
        user.unwrap(),
        "--",
        "env",
    ]);

    add_env_args(
        &mut cmd,
        session,
    );

    cmd.arg(command);
    cmd.args(args);

    cmd.output()
        .context(
            "failed to execute clipboard command as session user",
        )
}

fn add_env_args(
    command: &mut Command,
    session: &ClipboardSession,
) {
    if let Some(runtime) =
        &session.runtime_dir
    {
        command.arg(format!(
            "XDG_RUNTIME_DIR={}",
            runtime
        ));
    }

    if let Some(display) =
        &session.wayland_display
    {
        command.arg(format!(
            "WAYLAND_DISPLAY={}",
            display
        ));
    }

    if let Some(display) =
        &session.display
    {
        command.arg(format!(
            "DISPLAY={}",
            display
        ));
    }

    if let Some(xauthority) =
        &session.xauthority
    {
        command.arg(format!(
            "XAUTHORITY={}",
            xauthority
        ));
    }
}

fn apply_session_env(
    command: &mut Command,
    session: &ClipboardSession,
) {
    if let Some(runtime) =
        &session.runtime_dir
    {
        command.env(
            "XDG_RUNTIME_DIR",
            runtime,
        );
    }

    if let Some(display) =
        &session.display
    {
        command.env(
            "DISPLAY",
            display,
        );
    }

    if let Some(wayland) =
        &session.wayland_display
    {
        command.env(
            "WAYLAND_DISPLAY",
            wayland,
        );
    }

    if let Some(xauthority) =
        &session.xauthority
    {
        command.env(
            "XAUTHORITY",
            xauthority,
        );
    }
}

/*
 * ========================================================================
 * WAYLAND DISPLAY
 * ========================================================================
 */

fn find_wayland_display(
    runtime_dir: &str,
) -> Option<String> {
    let entries =
        std::fs::read_dir(
            runtime_dir,
        )
            .ok()?;

    for entry in
        entries.flatten()
    {
        let file_name =
            entry
                .file_name();

        let name =
            file_name
                .to_string_lossy();

        if name.starts_with(
            "wayland-",
        ) {
            return Some(
                name.into_owned(),
            );
        }
    }

    None
}

/*
 * ========================================================================
 * READ CLIPBOARD
 * ========================================================================
 */

fn read_clipboard(
    backend: ClipboardBackend,
    session: &ClipboardSession,
) -> Result<String> {
    let output =
        match backend {
            ClipboardBackend::Wayland => {
                run_session_command(
                    session,
                    "wl-paste",
                    &[
                        "--no-newline",
                    ],
                )?
            }

            ClipboardBackend::X11Xclip => {
                run_session_command(
                    session,
                    "xclip",
                    &[
                        "-selection",
                        "clipboard",
                        "-out",
                    ],
                )?
            }

            ClipboardBackend::X11Xsel => {
                run_session_command(
                    session,
                    "xsel",
                    &[
                        "--clipboard",
                        "--output",
                    ],
                )?
            }
        };

    if !output
        .status
        .success()
    {
        bail!(
            "clipboard read failed: {}",
            String::from_utf8_lossy(
                &output.stderr,
            )
        );
    }

    String::from_utf8(
        output.stdout,
    )
        .context(
            "clipboard is not valid UTF-8",
        )
}

/*
 * ========================================================================
 * WRITE CLIPBOARD
 * ========================================================================
 */

fn write_clipboard(
    backend: ClipboardBackend,
    session: &ClipboardSession,
    text: &str,
) -> Result<()> {
    let command =
        match backend {
            ClipboardBackend::Wayland =>
                "wl-copy",

            ClipboardBackend::X11Xclip =>
                "xclip",

            ClipboardBackend::X11Xsel =>
                "xsel",
        };

    let args =
        match backend {
            ClipboardBackend::Wayland =>
                &[
                    "--type",
                    "text/plain",
                ][..],

            ClipboardBackend::X11Xclip =>
                &[
                    "-selection",
                    "clipboard",
                ][..],

            ClipboardBackend::X11Xsel =>
                &[
                    "--clipboard",
                    "--input",
                ][..],
        };

    let user =
        session
            .user
            .as_deref();

    let mut child;

    /*
     * Если user отсутствует, пишем от текущего пользователя.
     */
    if user.is_none() {
        let mut cmd =
            Command::new(
                command,
            );

        apply_session_env(
            &mut cmd,
            session,
        );

        child =
            cmd.args(args)
                .stdin(
                    Stdio::piped(),
                )
                .stdout(
                    Stdio::null(),
                )
                .stderr(
                    Stdio::piped(),
                )
                .spawn()
                .context(
                    "failed to start clipboard writer",
                )?;
    } else {
        /*
         * Через runuser запускаем clipboard backend
         * внутри графической сессии пользователя.
         */
        let mut cmd =
            Command::new(
                "runuser",
            );

        cmd.args([
            "-u",
            user.unwrap(),
            "--",
            "env",
        ]);

        add_env_args(
            &mut cmd,
            session,
        );

        cmd.arg(
            command,
        );

        cmd.args(
            args,
        );

        child =
            cmd.stdin(
                Stdio::piped(),
            )
                .stdout(
                    Stdio::null(),
                )
                .stderr(
                    Stdio::piped(),
                )
                .spawn()
                .context(
                    "failed to start clipboard writer as session user",
                )?;
    }

    /*
     * Пишем содержимое clipboard через stdin.
     */
    {
        let stdin =
            child
                .stdin
                .as_mut()
                .context(
                    "clipboard writer stdin unavailable",
                )?;

        stdin
            .write_all(
                text.as_bytes(),
            )
            .context(
                "failed to write clipboard",
            )?;
    }

    let output =
        child
            .wait_with_output()
            .context(
                "failed to wait for clipboard writer",
            )?;

    if !output
        .status
        .success()
    {
        bail!(
            "clipboard write failed: {}",
            String::from_utf8_lossy(
                &output.stderr,
            )
        );
    }

    Ok(())
}

/*
 * ========================================================================
 * HASH
 * ========================================================================
 */

#[inline]
pub fn hash_text(
    text: &str,
) -> u64 {
    let mut hash =
        0xcbf29ce484222325u64;

    for byte in
        text.as_bytes()
    {
        hash ^=
            *byte as u64;

        hash =
            hash.wrapping_mul(
                0x100000001b3,
            );
    }

    hash
}