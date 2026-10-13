//! `revs://` links from the blame popup.
//!
//! Zed opens non-web links through macOS, which launches this binary from `Revs.app`
//! and delivers the link as a "get URL" Apple event. AppKit is loaded only in this mode
//! (via `dlopen`), so the language server process never maps it.

use crate::actions;
use std::ffi::{CStr, c_char, c_void};
use std::time::Duration;

type Id = *mut c_void;
type Sel = *const c_void;

#[link(name = "objc", kind = "dylib")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    fn objc_registerClassPair(class: Id);
    fn class_addMethod(class: Id, name: Sel, imp: *const c_void, types: *const c_char) -> bool;
}

const INTERNET_EVENT_CLASS: u32 = u32::from_be_bytes(*b"GURL");
const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const ACTIVATION_POLICY_PROHIBITED: i64 = 2;

unsafe fn sel(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn send(receiver: Id, selector: Sel) -> Id {
    let send: extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const c_void) };
    send(receiver, selector)
}

extern "C" fn handle_get_url(_this: Id, _cmd: Sel, event: Id, _reply: Id) {
    let url = unsafe {
        let param: extern "C" fn(Id, Sel, u32) -> Id =
            std::mem::transmute(objc_msgSend as *const c_void);
        let descriptor = param(event, sel(c"paramDescriptorForKeyword:"), DIRECT_OBJECT);
        let string = send(descriptor, sel(c"stringValue"));
        let utf8 = send(string, sel(c"UTF8String")) as *const c_char;
        (!utf8.is_null()).then(|| CStr::from_ptr(utf8).to_string_lossy().into_owned())
    };
    let status = match url.map(|url| actions::handle_url(&url)) {
        Some(Ok(())) => 0,
        Some(Err(error)) => {
            eprintln!("revs: {error}");
            1
        }
        None => 1,
    };
    std::process::exit(status);
}

pub fn run() -> i32 {
    unsafe {
        let appkit = libc::dlopen(
            c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(),
            libc::RTLD_LAZY,
        );
        if appkit.is_null() {
            return 1;
        }
        let app = send(objc_getClass(c"NSApplication".as_ptr()), sel(c"sharedApplication"));
        let set_policy: extern "C" fn(Id, Sel, i64) -> bool =
            std::mem::transmute(objc_msgSend as *const c_void);
        set_policy(app, sel(c"setActivationPolicy:"), ACTIVATION_POLICY_PROHIBITED);

        let class = objc_allocateClassPair(
            objc_getClass(c"NSObject".as_ptr()),
            c"RevsURLHandler".as_ptr(),
            0,
        );
        class_addMethod(
            class,
            sel(c"handleGetURL:withReplyEvent:"),
            handle_get_url as *const c_void,
            c"v@:@@".as_ptr(),
        );
        objc_registerClassPair(class);
        let handler = send(send(class, sel(c"alloc")), sel(c"init"));

        let manager = send(
            objc_getClass(c"NSAppleEventManager".as_ptr()),
            sel(c"sharedAppleEventManager"),
        );
        let set_handler: extern "C" fn(Id, Sel, Id, Sel, u32, u32) =
            std::mem::transmute(objc_msgSend as *const c_void);
        set_handler(
            manager,
            sel(c"setEventHandler:andSelector:forEventClass:andEventID:"),
            handler,
            sel(c"handleGetURL:withReplyEvent:"),
            INTERNET_EVENT_CLASS,
            GET_URL,
        );

        // Launched without a link (e.g. double-clicked): quit shortly.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(10));
            std::process::exit(0);
        });
        send(app, sel(c"run"));
    }
    0
}
