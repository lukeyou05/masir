use clap::Parser;
use color_eyre::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use windows::core::Result as WindowsCrateResult;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT_MOUSE;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::RealGetWindowClassW;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
use windows::Win32::UI::WindowsAndMessaging::HWND_TOP;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE;
use windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW;
use winput::message_loop;
use winput::message_loop::Event;

const CLASS_ALLOWLIST: [&str; 5] = [
    "Chrome_RenderWidgetHostHWND",                 // gross electron apps
    "Microsoft.UI.Content.DesktopChildSiteBridge", // windows explorer main panel
    "SysTreeView32",                               // windows explorer side panel
    "TITLE_BAR_SCAFFOLDING_WINDOW_CLASS",          // windows explorer side panel
    "DirectUIHWND",                                // windows explorer after interaction
];

const CLASS_BLOCKLIST: [&str; 5] = [
    "SHELLDLL_DefView",           // desktop window
    "Shell_TrayWnd",              // tray
    "TrayNotifyWnd",              // tray
    "MSTaskSwWClass",             // start bar icons
    "Windows.UI.Core.CoreWindow", // start menu
];

#[derive(Parser)]
#[clap(author, about, version)]
struct Opts {
    /// Path to a file with known focus-able HWNDs (e.g. komorebi.hwnd.json)
    #[clap(long)]
    hwnds: Option<PathBuf>,
}

fn main() -> Result<()> {
    let opts: Opts = Opts::parse();

    let hwnds = match opts.hwnds {
        None => {
            let hwnds: PathBuf = dirs::data_local_dir()
                .expect("there is no local data directory")
                .join("komorebi")
                .join("komorebi.hwnd.json");

            // TODO: We can add checks for other window managers here

            if hwnds.is_file() {
                Some(hwnds)
            } else {
                None
            }
        }
        Some(hwnds) => {
            if hwnds.is_file() {
                Some(hwnds)
            } else {
                None
            }
        }
    };

    if std::env::var("RUST_LIB_BACKTRACE").is_err() {
        std::env::set_var("RUST_LIB_BACKTRACE", "1");
    }

    color_eyre::install()?;

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt::Subscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )?;

    listen_for_movements(hwnds.clone());

    match hwnds {
        None => tracing::info!("masir is now running"),
        Some(hwnds) => tracing::info!(
            "masir is now running, and additionally checking hwnds against {}",
            hwnds.display()
        ),
    }

    let (ctrlc_sender, ctrlc_receiver) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        ctrlc_sender
            .send(())
            .expect("could not send signal on ctrl-c channel");
    })?;

    ctrlc_receiver
        .recv()
        .expect("could not receive signal on ctrl-c channel");

    tracing::info!("received ctrl-c, exiting");

    Ok(())
}

pub fn listen_for_movements(hwnds: Option<PathBuf>) {
    std::thread::spawn(move || {
        let receiver = message_loop::start().expect("could not start winput message loop");
        let mut eligibility_cache = HashMap::new();
        let mut map_instantiation_time = Instant::now();
        let map_max_age = Duration::from_secs(60) * 10; // 10 minutes

        loop {
            // clear our cache every 10 minutes
            if map_instantiation_time.elapsed() > map_max_age {
                tracing::info!("clearing hwnd cache, max map age is >10 minutes");
                eligibility_cache = HashMap::new();
                map_instantiation_time = Instant::now();
            }

            if let Event::MouseMoveRelative { .. } = receiver.next_event() {
                if let (Ok(cursor_pos_hwnd), Ok(foreground_hwnd)) =
                    (window_at_cursor_pos(), foreground_window())
                {
                    if cursor_pos_hwnd != foreground_hwnd {
                        let mut should_raise = false;
                        let mut should_cache = false;

                        // check our cache first
                        if let Some(eligible) = eligibility_cache.get(&cursor_pos_hwnd) {
                            if *eligible {
                                should_raise = true;
                                tracing::debug!(
                                    "hwnd {cursor_pos_hwnd} was found as eligible in the cache"
                                );
                            }
                        } else {
                            should_cache = true;
                        }

                        if !should_raise {
                            // step one: test against known classes
                            if let (Ok(cursor_pos_class), Ok(foreground_class)) = (
                                real_window_class_w(cursor_pos_hwnd),
                                real_window_class_w(foreground_hwnd),
                            ) {
                                // windows explorer related focus loop weirdness fixes in this block
                                {
                                    if cursor_pos_class == "DirectUIHWND"
                                        && foreground_class == "CabinetWClass"
                                    {
                                        continue;
                                    }

                                    if cursor_pos_class
                                        == "Microsoft.UI.Content.DesktopChildSiteBridge"
                                        && foreground_class == "CabinetWClass"
                                    {
                                        continue;
                                    }
                                }

                                // fail fast, exit this iteration of the loop and avoid any processing
                                // if we hit a blocklist entry
                                if CLASS_BLOCKLIST.contains(&&*cursor_pos_class) {
                                    tracing::debug!(
                                        "window class {cursor_pos_class} is blocklisted"
                                    );
                                    continue;
                                }

                                if CLASS_ALLOWLIST.contains(&&*cursor_pos_class) {
                                    tracing::debug!(
                                        "window class {cursor_pos_class} is allowlisted"
                                    );
                                    should_raise = true;
                                }

                                if !should_raise {
                                    tracing::trace!("window class is {cursor_pos_class}");
                                }
                            }

                            // step two: if available, test against known hwnds
                            if !should_raise {
                                if let Some(hwnds) = &hwnds {
                                    if let Ok(raw_hwnds) = std::fs::read_to_string(hwnds) {
                                        if raw_hwnds.contains(&cursor_pos_hwnd.to_string()) {
                                            tracing::debug!(
                                                "hwnd {cursor_pos_hwnd} was found in {}",
                                                hwnds.display()
                                            );
                                            should_raise = true;
                                        }
                                    }
                                }
                            }
                        }

                        if should_raise {
                            if should_cache {
                                eligibility_cache.insert(cursor_pos_hwnd, true);
                            }

                            match raise_and_focus_window(cursor_pos_hwnd) {
                                Ok(_) => {
                                    tracing::info!("raised hwnd {cursor_pos_hwnd}");
                                }
                                Err(error) => {
                                    tracing::error!(
                                        "failed to raise hwnd {cursor_pos_hwnd}: {error}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    });
}

macro_rules! as_ptr {
    ($value:expr) => {
        $value as *mut core::ffi::c_void
    };
}

pub enum WindowsResult<T, E> {
    Err(E),
    Ok(T),
}

macro_rules! impl_from_integer_for_windows_result {
    ( $( $integer_type:ty ),+ ) => {
        $(
            impl From<$integer_type> for WindowsResult<$integer_type, color_eyre::eyre::Error> {
                fn from(return_value: $integer_type) -> Self {
                    match return_value {
                        0 => Self::Err(std::io::Error::last_os_error().into()),
                        _ => Self::Ok(return_value),
                    }
                }
            }
        )+
    };
}

impl_from_integer_for_windows_result!(usize, isize, u16, u32, i32);

impl<T, E> From<WindowsResult<T, E>> for Result<T, E> {
    fn from(result: WindowsResult<T, E>) -> Self {
        match result {
            WindowsResult::Err(error) => Err(error),
            WindowsResult::Ok(ok) => Ok(ok),
        }
    }
}

pub trait ProcessWindowsCrateResult<T> {
    fn process(self) -> Result<T>;
}

macro_rules! impl_process_windows_crate_integer_wrapper_result {
    ( $($input:ty => $deref:ty),+ $(,)? ) => (
        paste::paste! {
            $(
                impl ProcessWindowsCrateResult<$deref> for $input {
                    fn process(self) -> Result<$deref> {
                        if self == $input(std::ptr::null_mut()) {
                            Err(std::io::Error::last_os_error().into())
                        } else {
                            Ok(self.0 as $deref)
                        }
                    }
                }
            )+
        }
    );
}

impl_process_windows_crate_integer_wrapper_result!(
    HWND => isize,
);

impl<T> ProcessWindowsCrateResult<T> for WindowsCrateResult<T> {
    fn process(self) -> Result<T> {
        match self {
            Ok(value) => Ok(value),
            Err(error) => Err(error.into()),
        }
    }
}
pub fn window_from_point(point: POINT) -> Result<isize> {
    unsafe { WindowFromPoint(point) }.process()
}

pub fn window_at_cursor_pos() -> Result<isize> {
    window_from_point(cursor_pos()?)
}

pub fn foreground_window() -> Result<isize> {
    unsafe { GetForegroundWindow() }.process()
}

pub fn cursor_pos() -> Result<POINT> {
    let mut cursor_pos = POINT::default();
    unsafe { GetCursorPos(&mut cursor_pos) }.process()?;

    Ok(cursor_pos)
}

pub fn raise_and_focus_window(hwnd: isize) -> Result<()> {
    let event = [INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    }];

    unsafe {
        // Send an input event to our own process first so that we pass the
        // foreground lock check
        SendInput(&event, size_of::<INPUT>() as i32);
        // Error ignored, as the operation is not always necessary.
        let _ = SetWindowPos(
            HWND(as_ptr!(hwnd)),
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        )
        .process();
        SetForegroundWindow(HWND(as_ptr!(hwnd)))
    }
    .ok()
    .process()
}

pub fn real_window_class_w(hwnd: isize) -> Result<String> {
    const BUF_SIZE: usize = 512;
    let mut class: [u16; BUF_SIZE] = [0; BUF_SIZE];

    let len = Result::from(WindowsResult::from(unsafe {
        RealGetWindowClassW(HWND(as_ptr!(hwnd)), &mut class)
    }))?;

    Ok(String::from_utf16(&class[0..len as usize])?)
}
