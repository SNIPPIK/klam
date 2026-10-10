#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("klam supports only Linux and Windows");

mod client;
mod config;
mod cursor;
mod discovery;
mod gui;
mod input;
#[cfg(windows)]
mod keymap;
mod protocol;
mod protocols;
mod server;
mod wm;

use anyhow::{
    Context,
    Result,
};
use clap::{
    Parser,
    Subcommand,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "klam",
    version,
    about = "Distributed keyboard and mouse sharing",
    long_about = "\
KLAM — distributed keyboard and mouse sharing for Linux.

Klam connects multiple computers over TCP and allows one physical
keyboard and mouse to control several machines as if they were one
continuous desktop.

The server is the central machine. Clients are attached to the
server on one of four sides:

    left
    right
    top
    bottom

When the cursor reaches a configured edge, ownership is transferred
to the corresponding client. Keyboard input follows the active
cursor.

Input capture and injection use evdev/uinput on Linux and
low-level hooks/SendInput on Windows.

Clipboard synchronization is also supported between connected
machines.

Run without a command to open the graphical interface.

COMMANDS:

    server    Start the central Klam server
    client    Connect a machine to a Klam server

Use:

    klam server --help
    klam client --help

for detailed command-specific help.
"
)]
struct Cli {
    /// Внутренний флаг: завершиться, когда закроется stdin (его ставит GUI).
    #[arg(long, hide = true, global = true)]
    exit_on_stdin_eof: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Start the central Klam server",
        long_about = "\
Start the central Klam server.

The server owns the physical input device and acts as the center of
the Klam topology.

Connected clients can be registered on:

    left
    right
    top
    bottom

The server keeps track of cursor ownership. While the server owns
the cursor, local input is processed locally. When the cursor reaches
a configured edge, ownership can be transferred to the corresponding
client.

The server listens for client connections on TCP.

EXAMPLES:

    # Listen on all interfaces, using a 2560x1600 screen
    klam server \\
        --screen-width 2560 \\
        --screen-height 1600

    # Listen on a specific address
    klam server \\
        --bind 192.168.1.143:24801 \\
        --screen-width 2560 \\
        --screen-height 1600

    # Explicitly select input devices
    klam server \\
        --device /dev/input/event5 \\
        --device /dev/input/event2 \\
        --screen-width 2560 \\
        --screen-height 1600

The server normally discovers useful input devices automatically.
Use --device when explicit device selection is required.
"
    )]
    Server {
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "0.0.0.0:24801",
            help = "TCP address and port to listen on",
            long_help = "\
TCP address and port used by the Klam server.

The default value:

    0.0.0.0:24801

means that Klam accepts TCP connections on port 24801 from all
available network interfaces.

Examples:

    --bind 0.0.0.0:24801
    --bind 192.168.1.143:24801
    --bind 127.0.0.1:24801
"
        )]
        bind: String,

        #[arg(
            long = "device",
            value_name = "PATH",
            action = clap::ArgAction::Append,
            help = "Use a specific /dev/input/event* device",
            long_help = "\
Explicitly select an input device.

This option can be specified multiple times.

When no devices are specified, Klam automatically discovers useful
keyboard and mouse devices.

Examples:

    --device /dev/input/event5
    --device /dev/input/event5
    --device /dev/input/event2

For normal operation, automatic discovery is recommended.
"
        )]
        device: Vec<PathBuf>,

        #[arg(
            long,
            value_name = "PIXELS",
            help = "Width of the server screen in pixels",
            long_help = "\
Width of the server screen in pixels.

This is the logical width used by the cursor tracker and by coordinate
mapping during server/client transitions.

Required on Linux. On Windows it is detected automatically when
omitted (size of the whole virtual desktop).

Example:

    --screen-width 2560
"
        )]
        screen_width: Option<i32>,

        #[arg(
            long,
            value_name = "PIXELS",
            help = "Height of the server screen in pixels",
            long_help = "\
Height of the server screen in pixels.

This is the logical height used by the cursor tracker and by
coordinate mapping during server/client transitions.

Required on Linux. On Windows it is detected automatically when
omitted (size of the whole virtual desktop).

Example:

    --screen-height 1600
"
        )]
        screen_height: Option<i32>,

        #[arg(
            long,
            value_name = "VALUE",
            default_value_t = 1.0,
            help = "Mouse sensitivity multiplier",
            long_help = "\
Mouse sensitivity multiplier.

The value is stored as a floating-point multiplier.

Default:

    1.0

Examples:

    --mouse-sensitivity 1.0
    --mouse-sensitivity 0.75
    --mouse-sensitivity 1.5
"
        )]
        mouse_sensitivity: f32,
    },

    #[command(
        about = "Connect a machine to a Klam server",
        long_about = "\
Connect this machine to a Klam server.

A client is positioned relative to the server using --direction.

The direction describes where THIS CLIENT is located relative to
the SERVER:

    left
    right
    top
    bottom

For example:

    server
      ┌───────────────┐
      │               │
      │     SERVER    │
      │               │
      └───────────────┘
              │
              ▼
            CLIENT

uses:

    --direction bottom

For a client located to the left of the server:

    CLIENT | SERVER

use:

    --direction left

The client receives keyboard/mouse ownership transitions from the
server and injects remote input (uinput on Linux, SendInput on
Windows).

Clipboard synchronization is also handled automatically.

EXAMPLES:

    # Client located to the left of the server
    klam client \\
        --connect 192.168.1.143:24801 \\
        --direction left \\
        --screen-width 2560 \\
        --screen-height 1600

    # Client located to the right
    klam client \\
        --connect 192.168.1.143:24801 \\
        --direction right \\
        --screen-width 1920 \\
        --screen-height 1080

    # Client located above the server
    klam client \\
        --connect 192.168.1.143:24801 \\
        --direction top \\
        --screen-width 2560 \\
        --screen-height 1440

    # Client located below the server
    klam client \\
        --connect 192.168.1.143:24801 \\
        --direction bottom \\
        --screen-width 1920 \\
        --screen-height 1080

If both screen dimensions are supplied, they are used directly.
If they are omitted, Klam attempts to detect the local screen size.
"
    )]
    Client {
        #[arg(
            long,
            value_name = "ADDR",
            help = "Address of the Klam server",
            long_help = "\
TCP address of the Klam server.

The address must point to the server's listening address.

Example:

    --connect 192.168.1.143:24801
"
        )]
        connect: String,

        #[arg(
            long,
            value_name = "DIRECTION",
            help = "Position of this client relative to the server",
            long_help = "\
Position of this client relative to the server.

Valid values:

    left
    right
    top
    bottom

The direction belongs to the CLIENT, not the server.

Examples:

    --direction left
    --direction right
    --direction top
    --direction bottom
"
        )]
        direction: protocol::Direction,

        #[arg(
            long,
            value_name = "PIXELS",
            help = "Local screen width in pixels",
            long_help = "\
Local client screen width in pixels.

When specified together with --screen-height, these values are used
directly and screen-size detection is skipped.

Example:

    --screen-width 2560
"
        )]
        screen_width: Option<i32>,

        #[arg(
            long,
            value_name = "PIXELS",
            help = "Local screen height in pixels",
            long_help = "\
Local client screen height in pixels.

When specified together with --screen-width, these values are used
directly and screen-size detection is skipped.

Example:

    --screen-height 1600
"
        )]
        screen_height: Option<i32>,
    },
}

/// Размер экрана сервера: явные аргументы, иначе автоопределение
/// (только Windows).
fn resolve_screen_size(
    width: Option<i32>,
    height: Option<i32>,
) -> Result<(i32, i32)> {
    let detected =
        wm::detect_screen_size();

    let width =
        width
            .or(detected.map(|size| size.0))
            .context(
                "--screen-width is required \
                 (automatic detection is not \
                 available on this platform)",
            )?;

    let height =
        height
            .or(detected.map(|size| size.1))
            .context(
                "--screen-height is required \
                 (automatic detection is not \
                 available on this platform)",
            )?;

    Ok((width, height))
}

#[tokio::main]
async fn main() -> Result<()> {
    wm::init_process();

    let cli =
        Cli::parse();

    if cli.exit_on_stdin_eof {
        std::thread::spawn(|| {
            use std::io::Read;

            let mut buffer = [0u8; 64];
            let mut stdin = std::io::stdin();

            while let Ok(read) = stdin.read(&mut buffer) {
                if read == 0 {
                    break;
                }
            }

            std::process::exit(0);
        });
    }

    let Some(command) = cli.command else {
        return gui::run();
    };

    match command {
        Command::Server {
            bind,
            device,
            screen_width,
            screen_height,
            mouse_sensitivity,
        } => {
            let (screen_width, screen_height) =
                resolve_screen_size(
                    screen_width,
                    screen_height,
                )?;

            server::run(
                config::ServerConfig {
                    bind,
                    devices: device,
                    screen_width,
                    screen_height,
                    mouse_sensitivity,
                },
            )
                .await?;
        }

        Command::Client {
            connect,
            direction,
            screen_width,
            screen_height,
        } => {
            client::run(
                connect,
                direction,
                screen_width,
                screen_height,
            )
                .await?;
        }
    }

    Ok(())
}