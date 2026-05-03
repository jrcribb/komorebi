use color_eyre::eyre;
use crossbeam_channel::Sender;
use crossbeam_channel::bounded;
use crossbeam_channel::unbounded;
use std::sync::OnceLock;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::LRESULT;
use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::Graphics::Dwm::DWM_THUMBNAIL_PROPERTIES;
use windows::Win32::Graphics::Dwm::DWM_TNP_OPACITY;
use windows::Win32::Graphics::Dwm::DWM_TNP_RECTDESTINATION;
use windows::Win32::Graphics::Dwm::DWM_TNP_SOURCECLIENTAREAONLY;
use windows::Win32::Graphics::Dwm::DWM_TNP_VISIBLE;
use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
use windows::Win32::UI::WindowsAndMessaging::DispatchMessageW;
use windows::Win32::UI::WindowsAndMessaging::HWND_TOP;
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::Win32::UI::WindowsAndMessaging::PM_REMOVE;
use windows::Win32::UI::WindowsAndMessaging::PeekMessageW;
use windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOREDRAW;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;
use windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW;
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
use windows::Win32::UI::WindowsAndMessaging::ShowWindow;
use windows::Win32::UI::WindowsAndMessaging::TranslateMessage;
use windows::Win32::UI::WindowsAndMessaging::WNDCLASSW;
use windows::core::PCWSTR;

use crate::WindowsApi;
use crate::core::Rect;
use crate::windows_api;

const GHOST_CLASS_NAME: &[u16] = &[
    b'k' as u16,
    b'o' as u16,
    b'm' as u16,
    b'o' as u16,
    b'r' as u16,
    b'e' as u16,
    b'b' as u16,
    b'i' as u16,
    b'-' as u16,
    b'g' as u16,
    b'h' as u16,
    b'o' as u16,
    b's' as u16,
    b't' as u16,
    0,
];

enum GhostCmd {
    Create {
        src_hwnd: isize,
        start_rect: Rect,
        z_above: Option<isize>,
        reply: Sender<eyre::Result<(isize, isize)>>,
    },
    UpdateRect {
        host_hwnd: isize,
        hthumb: isize,
        rect: Rect,
    },
    Destroy {
        host_hwnd: isize,
        hthumb: isize,
    },
}

struct GhostOwner {
    cmd_tx: Sender<GhostCmd>,
}

static GHOST_OWNER: OnceLock<GhostOwner> = OnceLock::new();

fn ghost_owner() -> &'static GhostOwner {
    GHOST_OWNER.get_or_init(|| {
        let (tx, rx) = unbounded::<GhostCmd>();
        std::thread::Builder::new()
            .name("komorebi-ghost-owner".into())
            .spawn(move || run_owner_loop(rx))
            .expect("failed to spawn ghost owner thread");
        GhostOwner { cmd_tx: tx }
    })
}

/// Eagerly initialise the ghost owner thread so the first movement animation
/// doesn't pay the spawn + class-registration cost. Idempotent. No-op for
/// users who never enable ghost movement only if it isn't called; calling
/// from a code path that's gated on `GHOST_MOVEMENT_ENABLED` keeps the lazy
/// guarantee.
pub fn prewarm() {
    let _ = ghost_owner();
}

extern "system" fn ghost_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn register_ghost_class() -> eyre::Result<()> {
    let h_module = WindowsApi::module_handle_w()?;
    let class_name = PCWSTR(GHOST_CLASS_NAME.as_ptr());
    let window_class = WNDCLASSW {
        hInstance: h_module.into(),
        lpszClassName: class_name,
        lpfnWndProc: Some(ghost_wnd_proc),
        ..Default::default()
    };
    // RegisterClassW returns 0 on failure with ERROR_CLASS_ALREADY_EXISTS as a
    // benign error if the class is already registered. We tolerate that.
    let _ = WindowsApi::register_class_w(&window_class);
    Ok(())
}

fn run_owner_loop(cmd_rx: crossbeam_channel::Receiver<GhostCmd>) {
    if let Err(error) = register_ghost_class() {
        tracing::error!("ghost owner: failed to register class: {error}");
        return;
    }

    loop {
        // Drain any pending Win32 messages (DWM/system messages destined for our hosts).
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        match cmd_rx.recv_timeout(Duration::from_millis(8)) {
            Ok(cmd) => handle_cmd(cmd),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_cmd(cmd: GhostCmd) {
    match cmd {
        GhostCmd::Create {
            src_hwnd,
            start_rect,
            z_above,
            reply,
        } => {
            let result = create_ghost(src_hwnd, start_rect, z_above);
            let _ = reply.send(result);
        }
        GhostCmd::UpdateRect {
            host_hwnd,
            hthumb,
            rect,
        } => {
            if let Err(error) = update_ghost(host_hwnd, hthumb, rect) {
                tracing::trace!("ghost owner: update failed: {error}");
            }
        }
        GhostCmd::Destroy { host_hwnd, hthumb } => {
            destroy_ghost(host_hwnd, hthumb);
        }
    }
}

fn instance_handle() -> eyre::Result<isize> {
    let h_module = WindowsApi::module_handle_w()?;
    Ok(h_module.0 as isize)
}

fn create_ghost(
    src_hwnd: isize,
    start_rect: Rect,
    z_above: Option<isize>,
) -> eyre::Result<(isize, isize)> {
    let class_name = PCWSTR(GHOST_CLASS_NAME.as_ptr());
    let host_hwnd = WindowsApi::create_ghost_host_window(class_name, instance_handle()?)?;

    // Position the host at start_rect (Rect uses left/top + width/height).
    let z_after = match z_above {
        Some(hwnd) => HWND(windows_api::as_ptr!(hwnd)),
        None => HWND_TOP,
    };
    let flags = SWP_NOACTIVATE | SWP_NOREDRAW | SWP_SHOWWINDOW;
    unsafe {
        let _ = SetWindowPos(
            HWND(windows_api::as_ptr!(host_hwnd)),
            Option::from(z_after),
            start_rect.left,
            start_rect.top,
            start_rect.right,
            start_rect.bottom,
            flags,
        );
    }

    let hthumb = match WindowsApi::dwm_register_thumbnail(host_hwnd, src_hwnd) {
        Ok(h) => h,
        Err(error) => {
            unsafe {
                let _ = DestroyWindow(HWND(windows_api::as_ptr!(host_hwnd)));
            }
            return Err(error);
        }
    };

    let props = thumbnail_properties(start_rect.right, start_rect.bottom);
    if let Err(error) = WindowsApi::dwm_update_thumbnail_properties(hthumb, &props) {
        let _ = WindowsApi::dwm_unregister_thumbnail(hthumb);
        unsafe {
            let _ = DestroyWindow(HWND(windows_api::as_ptr!(host_hwnd)));
        }
        return Err(error);
    }

    // Make the host visible. Layered/transparent ext styles ensure no input.
    unsafe {
        let _ = ShowWindow(
            HWND(windows_api::as_ptr!(host_hwnd)),
            SHOW_WINDOW_CMD(8), // SW_SHOWNA
        );
    }

    Ok((host_hwnd, hthumb))
}

fn update_ghost(host_hwnd: isize, hthumb: isize, rect: Rect) -> eyre::Result<()> {
    let flags: SET_WINDOW_POS_FLAGS = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW;
    unsafe {
        SetWindowPos(
            HWND(windows_api::as_ptr!(host_hwnd)),
            None,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            flags,
        )?;
    }

    let props = thumbnail_properties(rect.right, rect.bottom);
    WindowsApi::dwm_update_thumbnail_properties(hthumb, &props)
}

fn destroy_ghost(host_hwnd: isize, hthumb: isize) {
    let _ = WindowsApi::dwm_unregister_thumbnail(hthumb);
    unsafe {
        let _ = DestroyWindow(HWND(windows_api::as_ptr!(host_hwnd)));
    }
}

fn thumbnail_properties(width: i32, height: i32) -> DWM_THUMBNAIL_PROPERTIES {
    DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_VISIBLE
            | DWM_TNP_RECTDESTINATION
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rcSource: RECT::default(),
        opacity: 255,
        fVisible: true.into(),
        fSourceClientAreaOnly: false.into(),
    }
}

/// A live DWM-thumbnail "ghost" of a source window, used during movement
/// animations. While a ghost is active, the source window is typically cloaked
/// by the caller. The ghost is automatically disposed on drop, but callers
/// should prefer explicit `dispose()` to surface errors.
pub struct GhostWindow {
    host_hwnd: isize,
    hthumb: isize,
    disposed: bool,
}

impl GhostWindow {
    pub fn create(src_hwnd: isize, start_rect: Rect, z_above: Option<isize>) -> eyre::Result<Self> {
        let (reply_tx, reply_rx) = bounded::<eyre::Result<(isize, isize)>>(1);
        ghost_owner()
            .cmd_tx
            .send(GhostCmd::Create {
                src_hwnd,
                start_rect,
                z_above,
                reply: reply_tx,
            })
            .map_err(|e| eyre::eyre!("ghost owner channel send failed: {e}"))?;
        let (host_hwnd, hthumb) = reply_rx.recv()??;
        Ok(Self {
            host_hwnd,
            hthumb,
            disposed: false,
        })
    }

    pub fn host_hwnd(&self) -> isize {
        self.host_hwnd
    }

    pub fn update_rect(&self, rect: Rect) -> eyre::Result<()> {
        ghost_owner()
            .cmd_tx
            .send(GhostCmd::UpdateRect {
                host_hwnd: self.host_hwnd,
                hthumb: self.hthumb,
                rect,
            })
            .map_err(|e| eyre::eyre!("ghost owner channel send failed: {e}"))
    }

    /// Apply an opacity change directly via `DwmUpdateThumbnailProperties` on
    /// the calling thread. Unlike rect updates (which call `SetWindowPos` and
    /// therefore need the owner thread), opacity-only updates don't have
    /// thread affinity, and going through the channel introduces a race where
    /// the next `DwmFlush()` on the caller's thread can fire before the owner
    /// has processed the SetOpacity command — which collapses what should be
    /// a multi-frame fade into a single visible step.
    pub fn set_opacity(&self, opacity: u8) -> eyre::Result<()> {
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_OPACITY | DWM_TNP_VISIBLE,
            rcDestination: RECT::default(),
            rcSource: RECT::default(),
            opacity,
            fVisible: true.into(),
            fSourceClientAreaOnly: false.into(),
        };
        WindowsApi::dwm_update_thumbnail_properties(self.hthumb, &props)
    }

    pub fn dispose(mut self) -> eyre::Result<()> {
        self.dispose_inner()
    }

    fn dispose_inner(&mut self) -> eyre::Result<()> {
        if self.disposed {
            return Ok(());
        }
        self.disposed = true;
        ghost_owner()
            .cmd_tx
            .send(GhostCmd::Destroy {
                host_hwnd: self.host_hwnd,
                hthumb: self.hthumb,
            })
            .map_err(|e| eyre::eyre!("ghost owner channel send failed: {e}"))
    }
}

impl Drop for GhostWindow {
    fn drop(&mut self) {
        let _ = self.dispose_inner();
    }
}
