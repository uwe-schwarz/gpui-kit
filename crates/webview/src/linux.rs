//! Linux hosts Wry's WebKitGTK view in an X11 child window of the GPUI window.
//!
//! GTK cannot embed into a Wayland surface it does not own, so the GPUI application must
//! run on X11 (natively or through XWayland).

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    App, Bounds, Global, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    PlatformInput, Point, Window, point, px,
};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, WindowHandle, XlibWindowHandle,
};

/// How often pending GTK events are dispatched between GPUI frames.
const GTK_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// Marks that GTK is initialized and its main loop is driven by GPUI.
struct GtkMainLoop;

impl Global for GtkMainLoop {}

/// Initialize GTK on the X11 backend once and drive its main loop from GPUI.
pub(crate) fn ensure_gtk(cx: &mut App) -> wry::Result<()> {
    if cx.has_global::<GtkMainLoop>() {
        return Ok(());
    }

    if !gtk::is_initialized_main_thread() {
        // Wry downcasts the default GDK display to X11.
        gtk::gdk::set_allowed_backends("x11");
        gtk::init().map_err(|error| wry::Error::Io(std::io::Error::other(error)))?;
    }

    cx.set_global(GtkMainLoop);
    cx.spawn(async move |cx| {
        loop {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            cx.background_executor().timer(GTK_POLL_INTERVAL).await;
        }
    })
    .detach();

    Ok(())
}

/// WebKitGTK answers `prefers-color-scheme` from GTK's preference, which GTK 3 does not
/// read from the desktop, so without this every page renders light.
pub(crate) fn set_theme(theme: wry::Theme) {
    use gtk::prelude::GtkSettingsExt as _;
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(matches!(theme, wry::Theme::Dark));
    }
}

/// The GPUI window as the Xlib parent Wry requires; GPUI reports its X11 window through XCB.
pub(crate) struct X11Parent(XlibWindowHandle);

impl X11Parent {
    pub(crate) fn new(window: &Window) -> wry::Result<Self> {
        Self::from_raw(HasWindowHandle::window_handle(window)?.as_raw())
    }

    fn from_raw(handle: RawWindowHandle) -> wry::Result<Self> {
        match handle {
            RawWindowHandle::Xcb(handle) => {
                let mut xlib = XlibWindowHandle::new(handle.window.get().into());
                xlib.visual_id = handle.visual_id.map_or(0, |id| id.get().into());
                Ok(Self(xlib))
            }
            RawWindowHandle::Xlib(handle) => Ok(Self(handle)),
            _ => Err(wry::Error::Io(std::io::Error::other(
                "WebView on Linux requires an X11 window; start the application with \
                 `gpui_kit::platform::linux(WindowingModes::X11)`",
            ))),
        }
    }
}

impl HasWindowHandle for X11Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: The X11 window outlives the webview, which is destroyed with the GPUI window.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xlib(self.0)) })
    }
}

/// Wry scales bounds by GDK's factor, which ignores GPUI's, so pass device pixels.
pub(crate) fn device_bounds(bounds: Bounds<Pixels>, scale_factor: f32) -> wry::Rect {
    let device = |value: Pixels| f32::from(value) * scale_factor;
    wry::Rect {
        position: PhysicalPosition::new(device(bounds.origin.x), device(bounds.origin.y)).into(),
        size: PhysicalSize::new(device(bounds.size.width), device(bounds.size.height)).into(),
    }
}

/// Give the page the keyboard when it is pressed, and forward the press to GPUI, which does not
/// otherwise see it, so overlays that close on an outside click (popovers, menus) close when
/// the page is clicked.
pub(crate) fn forward_mouse_down(
    webview: &wry::WebView,
    origin: Rc<Cell<Point<Pixels>>>,
    window: &Window,
    cx: &App,
) {
    use gtk::{
        gdk::EventType,
        glib::Propagation,
        prelude::{Cast as _, WidgetExt as _},
    };
    use wry::WebViewExtUnix as _;

    let handle = window.window_handle();
    let cx = cx.to_async();
    webview
        .webview()
        .connect_button_press_event(move |widget, event| {
            let button = match event.button() {
                1 => MouseButton::Left,
                2 => MouseButton::Middle,
                3 => MouseButton::Right,
                _ => return Propagation::Proceed,
            };
            if event.event_type() != EventType::ButtonPress {
                return Propagation::Proceed;
            }
            keyboard::focus(widget.upcast_ref());
            let (x, y) = event.position();
            let gdk_scale = widget.scale_factor() as f32;
            let origin = origin.get();
            // GTK dispatches this from the GTK main loop; update GPUI once it is done.
            cx.spawn(async move |cx| {
                handle.update(cx, |_, window, cx| {
                    let scale = gdk_scale / window.scale_factor();
                    let position = origin + point(px(x as f32 * scale), px(y as f32 * scale));
                    window.dispatch_event(
                        PlatformInput::MouseDown(MouseDownEvent {
                            button,
                            position,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                            first_mouse: false,
                        }),
                        cx,
                    );
                    window.dispatch_event(
                        PlatformInput::MouseUp(MouseUpEvent {
                            button,
                            position,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                        }),
                        cx,
                    );
                })
            })
            .detach();
            Propagation::Proceed
        });
}

/// Zoom the page so its CSS pixels match GPUI's scale (GDK only supports integer scales),
/// times the page zoom the application asked for.
pub(crate) fn match_scale_factor(webview: &wry::WebView, scale_factor: f32, zoom: f64) {
    use gtk::prelude::WidgetExt as _;
    use wry::WebViewExtUnix as _;

    let gdk_scale = webview.webview().scale_factor().max(1);
    let _ = webview.zoom(f64::from(scale_factor) / f64::from(gdk_scale) * zoom);
}

/// A GPUI Fast composition surface hosting the webview: an X11 child window that GPUI cuts
/// its overlays out of, so they render above the page.
#[cfg(feature = "gpui-fast")]
pub(crate) struct NativeWebViewSurface {
    surface: gpui::WindowCompositionSurface,
    scale_factor: std::cell::Cell<f32>,
    window: gpui::AnyWindowHandle,
    cx: gpui::AsyncApp,
}

#[cfg(feature = "gpui-fast")]
impl NativeWebViewSurface {
    pub(crate) fn new(window: &Window, cx: &App) -> anyhow::Result<Self> {
        let surface = window
            .enable_window_composition()?
            .create_native_surface()?;
        Ok(Self {
            surface,
            scale_factor: std::cell::Cell::new(window.scale_factor()),
            window: window.window_handle(),
            cx: cx.to_async(),
        })
    }

    /// The surface's X11 window, as the parent Wry builds the webview in.
    pub(crate) fn parent(&self) -> wry::Result<X11Parent> {
        let handle = self
            .surface
            .platform_surface()
            .and_then(|surface| surface.platform_handle())
            .map_err(|error| wry::Error::Io(std::io::Error::other(error)))?;
        match handle.downcast::<RawWindowHandle>() {
            Ok(handle) => X11Parent::from_raw(*handle),
            Err(_) => Err(wry::Error::UnsupportedWindowHandle),
        }
    }

    pub(crate) fn set_scale_factor(&self, scale_factor: f32) {
        self.scale_factor.set(scale_factor);
    }

    /// Position the surface in the window and fill it with the webview.
    pub(crate) fn set_bounds(&self, webview: &wry::WebView, rect: wry::Rect) -> anyhow::Result<()> {
        use gpui::{DevicePixels, point, size};

        let scale_factor = f64::from(self.scale_factor.get());
        let position = rect.position.to_physical::<i32>(scale_factor);
        let extent = rect.size.to_physical::<i32>(scale_factor);
        self.surface.platform_surface()?.set_bounds(Bounds::new(
            point(DevicePixels(position.x), DevicePixels(position.y)),
            size(DevicePixels(extent.width), DevicePixels(extent.height)),
        ))?;
        webview.set_bounds(wry::Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(extent.width, extent.height).into(),
        })?;
        Ok(())
    }

    pub(crate) fn set_geometry(
        &self,
        webview: &wry::WebView,
        document: Bounds<Pixels>,
        visible: Bounds<Pixels>,
    ) -> anyhow::Result<()> {
        self.surface
            .platform_surface()?
            .set_bounds(visible.to_device_pixels(self.scale_factor.get()))?;
        let child = Bounds::new(document.origin - visible.origin, document.size);
        webview.set_bounds(device_bounds(child, self.scale_factor.get()))?;
        Ok(())
    }

    pub(crate) fn set_visible(&self, visible: bool) -> anyhow::Result<()> {
        self.surface.platform_surface()?.set_visible(visible)
    }

    pub(crate) fn release(self, webview: std::rc::Rc<wry::WebView>) {
        // Entity destruction can occur while App is borrowed. Destroy Wry's X11 windows before
        // removing the surface window that contains them.
        self.cx
            .clone()
            .spawn(async move |cx| {
                drop(webview);
                let result = self.window.update(cx, |_, window, _| {
                    window
                        .enable_window_composition()?
                        .remove_surface(self.surface.id())
                });
                if let Ok(Err(error)) = result {
                    log::warn!("cannot release WebView composition surface: {error:#}");
                }
            })
            .detach();
    }
}

/// The X11 window of a GPUI window, which hosts its webviews.
pub(crate) fn host_window(window: &Window) -> Option<std::ffi::c_ulong> {
    X11Parent::new(window).ok().map(|parent| parent.0.window)
}

/// Keyboard focus between GPUI and the webviews' GTK windows.
///
/// A webview is an X11 child window of its GPUI window. X11 sends key events to the window
/// with the input focus, but when that window is an ancestor of the window under the pointer,
/// the window under the pointer gets them. GTK selects key events on its windows (through
/// XInput2, so the event mask cannot be narrowed), so the keyboard followed the pointer: with
/// a GPUI input focused, typing over the page went to the page, and after a click into the
/// page, typing went to GPUI as soon as the pointer left it.
///
/// So a page takes the X input focus when it is clicked, and its GPUI window takes it back
/// when GPUI content is clicked or the webview is hidden. Key events that reach a page without
/// the X input focus are sent on to its GPUI window, through a GDK event handler that replaces
/// GTK's.
pub(crate) mod keyboard {
    use std::{
        cell::{Cell, RefCell},
        ffi::c_ulong,
    };

    use gdkx11::{X11Display, X11Window};
    use gtk::{
        gdk,
        glib::{self, translate::ToGlibPtr as _},
        prelude::*,
    };
    use x11::xlib;

    struct Page {
        widget: glib::WeakRef<gtk::Widget>,
        host: c_ulong,
    }

    thread_local! {
        static PAGES: RefCell<Vec<Page>> = const { RefCell::new(Vec::new()) };
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }

    /// The GTK widget of `webview`'s page.
    pub(crate) fn page(webview: &wry::WebView) -> gtk::Widget {
        use wry::WebViewExtUnix as _;
        webview.webview().upcast()
    }

    /// Track `page`, hosted by the GPUI window `host`.
    pub(crate) fn register(page: &gtk::Widget, host: c_ulong) {
        if !INSTALLED.replace(true) {
            gdk::Event::set_handler(Some(handle));
        }
        PAGES.with_borrow_mut(|pages| {
            pages.retain(|page| page.widget.upgrade().is_some());
            pages.push(Page {
                widget: page.downgrade(),
                host,
            });
        });
    }

    /// Give `page` the X input focus.
    pub(crate) fn focus(page: &gtk::Widget) {
        if let Some(xid) = page_xid(page)
            && let Some((display, xdisplay)) = display()
        {
            set_input_focus(&display, xdisplay, xid);
        }
    }

    /// Give `page` the X input focus if its GPUI window `host`, or a page in it, has it now.
    pub(crate) fn focus_if_active(page: &gtk::Widget, host: c_ulong) {
        let Some(xid) = page_xid(page) else {
            return;
        };
        let Some((display, xdisplay)) = display() else {
            return;
        };
        let focused = input_focus(xdisplay);
        if focused != xid && (focused == host || host_of(focused) == Some(host)) {
            set_input_focus(&display, xdisplay, xid);
        }
    }

    /// Give the X input focus back to the GPUI window `host`, if `page` has it.
    pub(crate) fn release(page: &gtk::Widget, host: c_ulong) {
        let Some((display, xdisplay)) = display() else {
            return;
        };
        if page_xid(page).is_some_and(|xid| input_focus(xdisplay) == xid) {
            set_input_focus(&display, xdisplay, host);
        }
    }

    fn handle(event: &mut gdk::Event) {
        if let Some(key) = event.downcast_ref::<gdk::EventKey>()
            && let Some(page) = event.window().and_then(|window| xid(&window.toplevel()))
            && let Some(host) = host_of(page)
            && let Some((display, xdisplay)) = display()
            && input_focus(xdisplay) != page
        {
            let press = event.event_type() == gdk::EventType::KeyPress;
            send_key(&display, xdisplay, host, key, press);
            return;
        }
        gtk::main_do_event(event);
    }

    /// The GPUI window hosting the page whose X11 window is `page`.
    fn host_of(page: c_ulong) -> Option<c_ulong> {
        PAGES.with_borrow(|pages| {
            pages.iter().find_map(|entry| {
                let widget = entry.widget.upgrade()?;
                (page_xid(&widget)? == page).then_some(entry.host)
            })
        })
    }

    /// The X11 window that receives the page's key events: GDK's toplevel window of the
    /// widget. (Wry's container window above it is foreign to GDK, and key events sent to it
    /// do not reach the page.)
    fn page_xid(page: &gtk::Widget) -> Option<c_ulong> {
        xid(&page.window()?.toplevel())
    }

    fn xid(window: &gdk::Window) -> Option<c_ulong> {
        Some(window.downcast_ref::<X11Window>()?.xid())
    }

    fn display() -> Option<(X11Display, *mut xlib::Display)> {
        let display = gdk::Display::default()?.downcast::<X11Display>().ok()?;
        // SAFETY: `display` is a live GdkX11Display; its Xlib connection lives as long as it.
        let xdisplay =
            unsafe { gdkx11::ffi::gdk_x11_display_get_xdisplay(display.to_glib_none().0) };
        Some((display, xdisplay))
    }

    fn input_focus(xdisplay: *mut xlib::Display) -> c_ulong {
        let (mut window, mut revert_to) = (0, 0);
        // SAFETY: A valid Xlib connection and out pointers.
        unsafe { xlib::XGetInputFocus(xdisplay, &mut window, &mut revert_to) };
        window
    }

    /// Set the X input focus on GTK's connection, after GTK's own pending requests.
    fn set_input_focus(display: &X11Display, xdisplay: *mut xlib::Display, xid: c_ulong) {
        display.error_trap_push();
        // SAFETY: A valid Xlib connection; errors (an unmapped window cannot take the focus)
        // are caught by the trap.
        unsafe { xlib::XSetInputFocus(xdisplay, xid, xlib::RevertToParent, xlib::CurrentTime) };
        display.error_trap_pop_ignored();
    }

    /// Send a key event that GTK received to the GPUI window `host` as a core X11 key event.
    fn send_key(
        display: &X11Display,
        xdisplay: *mut xlib::Display,
        host: c_ulong,
        key: &gdk::EventKey,
        press: bool,
    ) {
        // Modifier and button bits are the same in GDK and X11; XKB adds the group.
        let state = (key.state().bits() & 0x1fff) | (u32::from(key.group() & 3) << 13);
        let mut event = xlib::XEvent::from(xlib::XKeyEvent {
            type_: if press {
                xlib::KeyPress
            } else {
                xlib::KeyRelease
            },
            serial: 0,
            send_event: xlib::True,
            display: xdisplay,
            window: host,
            // SAFETY: A valid Xlib connection.
            root: unsafe { xlib::XDefaultRootWindow(xdisplay) },
            subwindow: 0,
            time: xlib::Time::from(key.time()),
            x: 0,
            y: 0,
            x_root: 0,
            y_root: 0,
            state,
            keycode: u32::from(key.hardware_keycode()),
            same_screen: xlib::True,
        });
        display.error_trap_push();
        // SAFETY: A valid Xlib connection and key event; errors (the window is gone) are caught
        // by the trap.
        unsafe {
            xlib::XSendEvent(xdisplay, host, xlib::False, xlib::NoEventMask, &mut event);
            xlib::XFlush(xdisplay);
        }
        display.error_trap_pop_ignored();
    }
}
