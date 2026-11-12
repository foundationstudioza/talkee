//! The dictation indicator: a small pill at the bottom of the screen.
//!
//! It says "Listening…" while recording and "Transcribing…" while the text is on its way, and stays
//! a few seconds with the reason when something went wrong or the text could only go on the
//! clipboard. On success it simply goes away: the text appearing is the confirmation.
//!
//! A plain Win32 window painted with GDI, never activated and transparent to clicks, so it can
//! never take focus from the app being dictated into. It lives on the main thread, whose loop pumps
//! its messages.

/// What the pill shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// The words.
    pub text: String,
    /// Accent colour of the dot, as 0xRRGGBB.
    pub accent: u32,
}

/// The pill. A no-op off Windows or when the window could not be made.
pub struct Overlay {
    #[cfg(windows)]
    inner: Option<win::Window>,
    current: Option<Shown>,
}

impl Overlay {
    /// Creates the (hidden) window.
    pub fn new() -> Self {
        Overlay {
            #[cfg(windows)]
            inner: win::Window::create(),
            current: None,
        }
    }

    /// Shows `shown`, or hides the pill for `None`. Cheap when nothing changed.
    pub fn set(&mut self, shown: Option<Shown>) {
        if shown == self.current {
            return;
        }
        #[cfg(windows)]
        if let Some(window) = &self.inner {
            match &shown {
                Some(s) => window.show(&s.text, s.accent),
                None => window.hide(),
            }
        }
        self.current = shown;
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
        EndPaint, FillRect, GetDC, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
        SetTextColor, SetWindowRgn, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CALCRECT, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD, HFONT, PAINTSTRUCT, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, SetLayeredWindowAttributes, SetWindowPos,
        ShowWindow, SystemParametersInfoW, HWND_TOPMOST, LWA_ALPHA, SPI_GETWORKAREA,
        SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WM_PAINT, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const HEIGHT: i32 = 40;
    const PAD: i32 = 18;
    const DOT: i32 = 10;
    const GAP: i32 = 10;
    const BACKGROUND: u32 = 0x1F1F1F;
    const FOREGROUND: u32 = 0xF2F2F2;

    /// What WM_PAINT draws. The window procedure has no user-data pointer of ours, and there is one
    /// overlay, so it lives here.
    static PAINT: Mutex<(Vec<u16>, u32)> = Mutex::new((Vec::new(), 0));

    fn rgb(colour: u32) -> COLORREF {
        // GDI wants 0x00BBGGRR.
        ((colour & 0xFF) << 16) | (colour & 0xFF00) | ((colour >> 16) & 0xFF)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Window {
        hwnd: HWND,
        font: HFONT,
    }

    impl Window {
        pub fn create() -> Option<Self> {
            let class = wide("TalkeeDictationOverlay");
            // SAFETY: a class with a valid window procedure and this module's instance. Failure to
            // register (already registered) is harmless: CreateWindowExW then uses the existing one.
            unsafe {
                let instance = GetModuleHandleW(std::ptr::null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    ..std::mem::zeroed()
                };
                RegisterClassW(&wc);
                let title = wide("Talkee dictation");
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE
                        | WS_EX_LAYERED
                        | WS_EX_TRANSPARENT,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    200,
                    HEIGHT,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return None;
                }
                SetLayeredWindowAttributes(hwnd, 0, 235, LWA_ALPHA);
                let face = wide("Segoe UI");
                let font = CreateFontW(
                    -17,
                    0,
                    0,
                    0,
                    FW_SEMIBOLD as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    face.as_ptr(),
                );
                Some(Window { hwnd, font })
            }
        }

        pub fn show(&self, text: &str, accent: u32) {
            let text = wide(text);
            // SAFETY: the window and font are ours; the DC is released; the region is handed to the
            // window, which then owns it.
            unsafe {
                // Measure the text to size the pill.
                let dc = GetDC(self.hwnd);
                let old = SelectObject(dc, self.font as _);
                let mut measure = RECT {
                    left: 0,
                    top: 0,
                    right: 2000,
                    bottom: HEIGHT,
                };
                DrawTextW(
                    dc,
                    text.as_ptr(),
                    -1,
                    &mut measure,
                    DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(dc, old);
                ReleaseDC(self.hwnd, dc);
                let width = (PAD + DOT + GAP + measure.right + PAD).clamp(160, 1200);

                let mut area: RECT = std::mem::zeroed();
                SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as *mut _, 0);
                let x = area.left + (area.right - area.left - width) / 2;
                let y = area.bottom - HEIGHT - 48;

                if let Ok(mut paint) = PAINT.lock() {
                    *paint = (text, accent);
                }
                SetWindowRgn(
                    self.hwnd,
                    CreateRoundRectRgn(0, 0, width + 1, HEIGHT + 1, HEIGHT, HEIGHT),
                    1,
                );
                SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, width, HEIGHT, SWP_NOACTIVATE);
                InvalidateRect(self.hwnd, std::ptr::null(), 1);
                ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }

        pub fn hide(&self) {
            // SAFETY: our window.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_PAINT {
            // SAFETY: standard paint handling on our own window; every GDI object made here is
            // deleted, and the font is only selected for the duration of the draw.
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
                let background = CreateSolidBrush(rgb(BACKGROUND));
                FillRect(dc, &rect, background);
                DeleteObject(background as _);

                let (text, accent) = PAINT.lock().map(|p| p.clone()).unwrap_or_default();
                let mid = rect.bottom / 2;
                let dot =
                    CreateRoundRectRgn(PAD, mid - DOT / 2, PAD + DOT, mid + DOT / 2, DOT, DOT);
                let accent_brush = CreateSolidBrush(rgb(accent));
                windows_sys::Win32::Graphics::Gdi::FillRgn(dc, dot, accent_brush);
                DeleteObject(accent_brush as _);
                DeleteObject(dot as _);

                let face = wide("Segoe UI");
                let font = CreateFontW(
                    -17,
                    0,
                    0,
                    0,
                    FW_SEMIBOLD as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    face.as_ptr(),
                );
                let old = SelectObject(dc, font as _);
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(dc, rgb(FOREGROUND));
                let mut text_rect = RECT {
                    left: PAD + DOT + GAP,
                    ..rect
                };
                if !text.is_empty() {
                    DrawTextW(
                        dc,
                        text.as_ptr(),
                        -1,
                        &mut text_rect,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }
                SelectObject(dc, old);
                DeleteObject(font as _);
                EndPaint(hwnd, &ps);
            }
            return 0;
        }
        // SAFETY: everything else goes to the default procedure with its own arguments.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}
