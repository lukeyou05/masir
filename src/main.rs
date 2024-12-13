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
use windows::Win32::UI::WindowsAndMessaging::GetAncestor;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongW;
use windows::Win32::UI::WindowsAndMessaging::RealGetWindowClassW;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
use windows::Win32::UI::WindowsAndMessaging::GA_ROOT;
use windows::Win32::UI::WindowsAndMessaging::GET_ANCESTOR_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::HWND_TOP;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE;
use windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use winput::message_loop;
use winput::message_loop::Event;
use winput::Action;

// ignore cursor_pos_hwnd if it is one of these classes
const CLASS_IGNORELIST: [&str; 5] = [
    "SHELLDLL_DefView",           // desktop window
    "Shell_TrayWnd",              // tray
    "TrayNotifyWnd",              // tray
    "MSTaskSwWClass",             // start bar icons
    "Windows.UI.Core.CoreWindow", // start menu
];

// prevent masir from raising any windows when the foreground window is one of these classes
const CLASS_PAUSELIST: [&str; 2] = [
    "XamlExplorerHostIslandWindow", // task switcher
    "ForegroundStaging",            // also task switcher
];

#[derive(Parser)]
#[clap(author, about, version)]
struct Opts {
    /// Enable komorebi integration and use its HWNDs file
    #[clap(long)]
    komorebi: bool,
    /// Path to a file with known focus-able HWNDs (e.g. komorebi.hwnd.json)
    #[clap(long)]
    hwnds: Option<PathBuf>,
}

fn main() -> Result<()> {
    let opts: Opts = Opts::parse();

    let hwnds = match opts.hwnds {
        None => {
            // TODO: We can add checks for other window managers here
            let hwnds_option: Option<PathBuf> = if opts.komorebi {
                Some(
                    dirs::data_local_dir()
                        .expect("there is no local data directory")
                        .join("komorebi")
                        .join("komorebi.hwnd.json"),
                )
            } else {
                None
            };

            hwnds_option.filter(|hwnds| hwnds.is_file())
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
        let mut class_cache: HashMap<isize, String> = HashMap::new();
        let mut hwnd_pair_cache: HashMap<isize, isize> = HashMap::new();
        let mut top_level_hwnd_cache: HashMap<isize, isize> = HashMap::new();

        let mut cache_instantiation_time = Instant::now();
        let max_cache_age = Duration::from_secs(60) * 10; // 10 minutes

        let mut old_cursor_pos = (0, 0);
        let mut is_mouse_down = false;

        loop {
            // clear our caches every 10 minutes
            if cache_instantiation_time.elapsed() > max_cache_age {
                tracing::info!("clearing caches, cache age is >10 minutes");

                eligibility_cache = HashMap::new();
                class_cache = HashMap::new();
                hwnd_pair_cache = HashMap::new();
                top_level_hwnd_cache = HashMap::new();

                cache_instantiation_time = Instant::now();
            }

            match receiver.next_event() {
                Event::MouseMoveRelative { x, y } => {
                    // TODO janky ass fix for apps like Flow Launcher since MouseMoveRelative can
                    // be sent when the foreground window changes, even if cursor position hasn't
                    if (x, y) == old_cursor_pos {
                        continue;
                    } else {
                        old_cursor_pos = (x, y);
                    }

                    // check if the mouse is being pressed (like when resizing a window)
                    if is_mouse_down {
                        continue;
                    }

                    if let (Ok(cursor_pos_hwnd), Ok(foreground_hwnd)) =
                        (window_at_cursor_pos(), foreground_window())
                    {
                        if cursor_pos_hwnd == foreground_hwnd {
                            continue;
                        }

                        let top_level_hwnd = match top_level_hwnd_cache.get(&cursor_pos_hwnd) {
                            Some(hwnd) => *hwnd,
                            None => match get_ancestor(cursor_pos_hwnd, GA_ROOT) {
                                Ok(hwnd) => {
                                    top_level_hwnd_cache.insert(cursor_pos_hwnd, hwnd);
                                    hwnd
                                }
                                Err(e) => {
                                    tracing::error!("could not get ancestor: {e}");
                                    cursor_pos_hwnd
                                }
                            },
                        };

                        if top_level_hwnd == foreground_hwnd {
                            continue;
                        }

                        if let Some(paired_hwnd) = hwnd_pair_cache.get(&cursor_pos_hwnd) {
                            if foreground_hwnd == *paired_hwnd {
                                tracing::trace!("hwnds {cursor_pos_hwnd} and {foreground_hwnd} are known to refer to the same application, skipping");
                                continue;
                            }
                        }

                        let mut should_raise = false;

                        // check our class cache to avoid syscalls
                        let mut cursor_pos_class = class_cache.get(&cursor_pos_hwnd).cloned();
                        let mut foreground_class = class_cache.get(&foreground_hwnd).cloned();

                        // make syscalls if necessary and populate the class cache
                        match &cursor_pos_class {
                            None => {
                                if let Ok(class) = real_window_class_w(cursor_pos_hwnd) {
                                    class_cache.insert(cursor_pos_hwnd, class.clone());
                                    cursor_pos_class = Some(class);
                                }
                            }
                            Some(class) => {
                                tracing::debug!(
                                    "hwnd {cursor_pos_hwnd} class was found in the cache: {class}"
                                );
                            }
                        }

                        // make syscalls if necessary and populate the class cache
                        match &foreground_class {
                            None => {
                                if let Ok(class) = real_window_class_w(foreground_hwnd) {
                                    class_cache.insert(foreground_hwnd, class.clone());
                                    foreground_class = Some(class);
                                }
                            }
                            Some(class) => {
                                tracing::debug!(
                                    "hwnd {foreground_hwnd} class was found in the cache: {class}"
                                );
                            }
                        }

                        if let (Some(cursor_pos_class), Some(foreground_class)) =
                            (&cursor_pos_class, &foreground_class)
                        {
                            // check if the foreground window is in the pause list (i.e. task switcher)
                            if CLASS_PAUSELIST.contains(&foreground_class.as_str()) {
                                continue;
                            }

                            // steam fixes - populate the hwnd pair cache if necessary
                            if cursor_pos_class == "Chrome_RenderWidgetHostHWND"
                                && foreground_class == "SDL_app"
                            {
                                hwnd_pair_cache.insert(cursor_pos_hwnd, foreground_hwnd);
                                continue;
                            }
                        }

                        // check our eligibility cache
                        if let Some(eligible) = eligibility_cache.get(&cursor_pos_hwnd) {
                            if *eligible {
                                should_raise = true;
                                tracing::debug!(
                                    "hwnd {cursor_pos_hwnd} was found as eligible in the cache"
                                );
                            }
                        } else if let Some(hwnds) = &hwnds {
                            // if the eligibility for this hwnd isn't cached, and if 'hwnds' is a
                            // valid file, then check against the hwnds in there
                            //
                            // supposedly, this should "ensure that only windows managed by the
                            // tiling window manager are eligible to be focused"

                            if let Ok(raw_hwnds) = std::fs::read_to_string(hwnds) {
                                if raw_hwnds.contains(&cursor_pos_hwnd.to_string())
                                    || raw_hwnds.contains(&top_level_hwnd.to_string())
                                {
                                    tracing::debug!(
                                            "hwnd {cursor_pos_hwnd} or {top_level_hwnd} was found in {}",
                                            hwnds.display()
                                        );

                                    eligibility_cache.insert(cursor_pos_hwnd, true);
                                    should_raise = true;
                                }
                                // gonna ignore the case where raw_hwnds doesn't contain either
                                // hwnds in case there is some delay when writing to the file
                            }
                        } else {
                            // otherwise, do some tests

                            // step one: test against known window styles
                            let has_filtered_style = has_filtered_style(top_level_hwnd);

                            // step two: test against known classes
                            let is_in_ignore_list = match cursor_pos_class {
                                Some(class) => CLASS_IGNORELIST.contains(&class.as_str()),
                                None => true,
                            };

                            let is_eligible = !has_filtered_style && !is_in_ignore_list;
                            eligibility_cache.insert(cursor_pos_hwnd, is_eligible);
                            should_raise = is_eligible;
                        }

                        if should_raise {
                            match raise_and_focus_window(top_level_hwnd) {
                                Ok(_) => {
                                    tracing::info!(
                                            "raised hwnd: {top_level_hwnd:#x}; cursor_pos_hwnd: {cursor_pos_hwnd:#x}"
                                        );
                                }
                                Err(error) => {
                                    tracing::error!(
                                        "failed to raise hwnd {top_level_hwnd:#x}: {error}"
                                    );
                                }
                            }
                        }
                    }
                }
                Event::MouseButton { action, .. } => match action {
                    Action::Press => is_mouse_down = true,
                    Action::Release => is_mouse_down = false,
                },
                _ => {}
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

// This method of checking window styles and caching HWNDs can fail if a window changes its window
// styles at some point after caching, but I'm not going to worry about that for now TODO
fn has_filtered_style(hwnd: isize) -> bool {
    //let style = unsafe { GetWindowLongW(HWND(as_ptr!(hwnd)), GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(HWND(as_ptr!(hwnd)), GWL_EXSTYLE) as u32 };

    ex_style & WS_EX_TOOLWINDOW.0 != 0 || ex_style & WS_EX_NOACTIVATE.0 != 0
}

fn get_ancestor(hwnd: isize, gaflags: GET_ANCESTOR_FLAGS) -> Result<isize> {
    unsafe { GetAncestor(HWND(as_ptr!(hwnd)), gaflags) }.process()
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
