//! The OS boundary: CoreGraphics display facts and reconfiguration callback, the distributed
//! unlock notification, NSScreen, and the NSApp run loop that makes CG callbacks arrive
//! (tickets 07 and 10: a bare CFRunLoop receives nothing — D24).

use crate::display::{Rect, Screen};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen};
use objc2_foundation::{NSNumber, NSString};
use std::ffi::{c_char, c_void};
use std::sync::{Mutex, OnceLock};

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type ReconfigCallback = extern "C" fn(display: u32, flags: u32, user: *mut c_void);
type NotificationCallback = extern "C" fn(*mut c_void, *mut c_void, *const c_void, *const c_void, *const c_void);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayRotation(display: u32) -> f64;
    fn CGDisplayIsBuiltin(display: u32) -> i32;
    fn CGDisplayIsMain(display: u32) -> i32;
    fn CGDisplayVendorNumber(display: u32) -> u32;
    fn CGDisplayModelNumber(display: u32) -> u32;
    fn CGDisplaySerialNumber(display: u32) -> u32;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut c_void);
    fn CGDisplayRegisterReconfigurationCallback(cb: ReconfigCallback, user: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFNotificationCenterGetDistributedCenter() -> *mut c_void;
    fn CFNotificationCenterAddObserver(
        center: *mut c_void,
        observer: *const c_void,
        cb: NotificationCallback,
        name: *const c_void,
        object: *const c_void,
        behavior: isize,
    );
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
}

unsafe extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_sync_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

const BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const DELIVER_IMMEDIATELY: isize = 4;

type Hook = Box<dyn Fn() + Send + Sync>;
static DISPLAY_HOOK: OnceLock<Hook> = OnceLock::new();
static UNLOCK_HOOK: OnceLock<Hook> = OnceLock::new();
/// (display id, origin x, origin y, width, height) per active display.
pub type Geometry = Vec<(u32, i64, i64, i64, i64)>;
static LAST_GEOMETRY: Mutex<Geometry> = Mutex::new(Vec::new());

/// Active displays with their bounds, sorted: D17's "count, origin, bounds".
pub fn geometry_fingerprint() -> Geometry {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) } != 0 {
        return vec![];
    }
    let mut out: Vec<_> = ids[..n as usize]
        .iter()
        .map(|&id| {
            let b = unsafe { CGDisplayBounds(id) };
            (id, b.origin.x as i64, b.origin.y as i64, b.size.width as i64, b.size.height as i64)
        })
        .collect();
    out.sort();
    out
}

extern "C" fn on_reconfigure(_display: u32, flags: u32, _user: *mut c_void) {
    if flags & BEGIN_CONFIGURATION_FLAG != 0 {
        return;
    }
    // HDR, Night Shift, brightness: callbacks with no topology change are ignored (D17).
    let now = geometry_fingerprint();
    let mut last = LAST_GEOMETRY.lock().unwrap_or_else(|e| e.into_inner());
    if *last == now {
        return;
    }
    *last = now;
    if let Some(hook) = DISPLAY_HOOK.get() {
        hook();
    }
}

/// Call once, on the main thread, before `run_app`.
pub fn install_display_callback(hook: impl Fn() + Send + Sync + 'static) {
    *LAST_GEOMETRY.lock().unwrap_or_else(|e| e.into_inner()) = geometry_fingerprint();
    assert!(DISPLAY_HOOK.set(Box::new(hook)).is_ok(), "display callback installed twice");
    let err = unsafe { CGDisplayRegisterReconfigurationCallback(on_reconfigure, std::ptr::null_mut()) };
    if err != 0 {
        crate::log!("CGDisplayRegisterReconfigurationCallback failed: {err}");
    }
}

extern "C" fn on_unlock(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
    if let Some(hook) = UNLOCK_HOOK.get() {
        hook();
    }
}

/// D23: `com.apple.screenIsUnlocked` is a verify-only trigger. Delivered on the main run loop.
pub fn observe_unlock(hook: impl Fn() + Send + Sync + 'static) {
    assert!(UNLOCK_HOOK.set(Box::new(hook)).is_ok(), "unlock observer installed twice");
    unsafe {
        let name = CFStringCreateWithCString(std::ptr::null(), c"com.apple.screenIsUnlocked".as_ptr(), CF_STRING_ENCODING_UTF8);
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            &raw const UNLOCK_HOOK as *const c_void,
            on_unlock,
            name,
            std::ptr::null(),
            DELIVER_IMMEDIATELY,
        );
    }
}

/// Run `f` on the main thread and return its result. NSScreen is main-thread-only.
fn on_main<R: Send, F: FnOnce() -> R + Send>(f: F) -> R {
    if unsafe { libc::pthread_main_np() } != 0 {
        return f();
    }
    struct Ctx<F, R> {
        f: Option<F>,
        r: Option<R>,
    }
    extern "C" fn trampoline<F: FnOnce() -> R, R>(p: *mut c_void) {
        let ctx = unsafe { &mut *(p as *mut Ctx<F, R>) };
        ctx.r = Some((ctx.f.take().expect("called once"))());
    }
    let mut ctx = Ctx { f: Some(f), r: None };
    unsafe {
        dispatch_sync_f(
            &raw const _dispatch_main_q as *const c_void,
            &mut ctx as *mut Ctx<F, R> as *mut c_void,
            trampoline::<F, R>,
        );
    }
    ctx.r.expect("the main queue ran the closure")
}

fn cg_screen(cg_id: u32, name: String) -> Screen {
    unsafe {
        let b = CGDisplayBounds(cg_id);
        let mode = CGDisplayCopyDisplayMode(cg_id);
        let (px_w, px_h) = if mode.is_null() {
            (0, 0)
        } else {
            let px = (CGDisplayModeGetPixelWidth(mode) as u64, CGDisplayModeGetPixelHeight(mode) as u64);
            CGDisplayModeRelease(mode);
            px
        };
        Screen {
            cg_id,
            name,
            bounds: Rect { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height },
            px_w,
            px_h,
            rotation: CGDisplayRotation(cg_id),
            main: CGDisplayIsMain(cg_id) != 0,
            builtin: CGDisplayIsBuiltin(cg_id) != 0,
            vendor: CGDisplayVendorNumber(cg_id),
            model: CGDisplayModelNumber(cg_id),
            serial: CGDisplaySerialNumber(cg_id),
        }
    }
}

/// Every NSScreen, in `NSScreen.screens` order — the order AeroSpace's
/// `%{monitor-appkit-nsscreen-screens-id}` indexes (plan P-1).
pub fn screens() -> Vec<Screen> {
    on_main(|| {
        let mtm = MainThreadMarker::new().expect("on the main thread");
        let key = NSString::from_str("NSScreenNumber");
        NSScreen::screens(mtm)
            .iter()
            .filter_map(|s| {
                let number = s.deviceDescription().objectForKey(&key)?.downcast::<NSNumber>().ok()?;
                Some(cg_screen(number.unsignedIntValue(), s.localizedName().to_string()))
            })
            .collect()
    })
}

/// The shared NSApplication with no Dock icon or menu bar (ticket 10: unbundled binary,
/// activation policy Prohibited).
pub fn shared_app() -> Retained<NSApplication> {
    let mtm = MainThreadMarker::new().expect("NSApplication lives on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app
}

/// Run the AppKit loop forever. `run` calls `finishLaunching` itself.
pub fn run_app() -> ! {
    let app = shared_app();
    app.run();
    std::process::exit(0)
}
