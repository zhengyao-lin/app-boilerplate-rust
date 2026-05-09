//! A wrapper around the asynchronous NBGL `nbgl_useCaseKeyboard` C API.
//!
//! Mirrors the shape of `ledger_device_sdk::nbgl::NbglKeypad`, but draws a
//! text-entry keyboard with a single confirmation button instead of a keypad.
//!
//! `nbgl_useCaseKeyboard`, `nbgl_keyboardParams_t`, `nbgl_kbdButtonParams_t`
//! and `nbgl_kbdSuggestParams_t` are all gated behind `#ifdef NBGL_KEYBOARD`
//! in the C SDK headers, so bindgen does not emit them. We declare them here
//! by hand. The C symbol is made linkable via `LEDGER_SDK_EXTRA_DEFINES =
//! "NBGL_KEYBOARD"` (see `.cargo/config.toml`).

#![allow(non_camel_case_types)]

extern crate alloc;

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicBool, Ordering};

use ledger_device_sdk::io::Comm;
use ledger_device_sdk::sys::{
    keyboardCase_t, keyboardMode_t, nbgl_callback_t, nbgl_keyboardButtonsCallback_t,
    nbgl_layoutKeyboardContentType_t, nbgl_layoutTouchCallback_t, ux_process_finger_event,
    ux_process_ticker_event, KEYBOARD_WITH_BUTTON, LOWER_CASE, MODE_LETTERS,
    OS_IO_PACKET_TYPE_SEPH, OS_IO_PACKET_TYPE_SE_EVT, SEPROXYHAL_TAG_FINGER_EVENT,
    SEPROXYHAL_TAG_TICKER_EVENT,
};

// FFI types that bindgen does not emit (gated behind NBGL_KEYBOARD).
// Layout mirrors `lib_nbgl/include/nbgl_use_case.h:328-374`.

#[repr(C)]
#[derive(Copy, Clone)]
struct nbgl_kbdButtonParams_t {
    button_text: *const c_char,
    on_button_callback: nbgl_callback_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct nbgl_kbdSuggestParams_t {
    buttons: *const *const c_char,
    first_button_token: c_int,
    on_button_callback: nbgl_layoutTouchCallback_t,
    update_buttons_callback: nbgl_keyboardButtonsCallback_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
union nbgl_keyboardParams_variant {
    suggestion_params: nbgl_kbdSuggestParams_t,
    confirmation_params: nbgl_kbdButtonParams_t,
}

#[repr(C)]
struct nbgl_keyboardParams_t {
    type_: nbgl_layoutKeyboardContentType_t,
    title: *const c_char,
    entry_buffer: *mut c_char,
    entry_max_len: u8,
    mode: keyboardMode_t,
    letters_only: bool,
    numbered: bool,
    number: u8,
    casing: keyboardCase_t,
    variant: nbgl_keyboardParams_variant,
}

unsafe extern "C" {
    fn nbgl_useCaseKeyboard(
        params: *const nbgl_keyboardParams_t,
        back_callback: nbgl_callback_t,
    );
}

// Static state shared with the C callbacks. The keyboard is meant to be shown
// one screen at a time, so a single global slot is sufficient.
const TEXT_BUFFER_SIZE: usize = 64;
static mut TEXT_BUFFER: [u8; TEXT_BUFFER_SIZE] = [0; TEXT_BUFFER_SIZE];
static KEYBOARD_DONE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_CONFIRMED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn confirm_cb() {
    KEYBOARD_CONFIRMED.store(true, Ordering::Release);
    KEYBOARD_DONE.store(true, Ordering::Release);
}

unsafe extern "C" fn back_cb() {
    KEYBOARD_DONE.store(true, Ordering::Release);
}

/// A builder to display a text-entry keyboard with a confirmation button.
pub struct NbglKeyboard {
    title: CString,
    button_text: CString,
    entry_max_len: u8,
    mode: keyboardMode_t,
    casing: keyboardCase_t,
    letters_only: bool,
}

impl Default for NbglKeyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl NbglKeyboard {
    pub fn new() -> Self {
        Self {
            title: CString::new("Enter text").unwrap(),
            button_text: CString::new("Confirm").unwrap(),
            entry_max_len: (TEXT_BUFFER_SIZE - 1) as u8,
            mode: MODE_LETTERS,
            casing: LOWER_CASE,
            letters_only: false,
        }
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = CString::new(title).unwrap();
        self
    }

    pub fn button_text(mut self, text: &str) -> Self {
        self.button_text = CString::new(text).unwrap();
        self
    }

    pub fn entry_max_len(mut self, max: u8) -> Self {
        self.entry_max_len = max.min((TEXT_BUFFER_SIZE - 1) as u8);
        self
    }

    pub fn mode(mut self, mode: keyboardMode_t) -> Self {
        self.mode = mode;
        self
    }

    pub fn casing(mut self, casing: keyboardCase_t) -> Self {
        self.casing = casing;
        self
    }

    pub fn letters_only(mut self, letters_only: bool) -> Self {
        self.letters_only = letters_only;
        self
    }

    /// Display the keyboard and block until the user confirms or backs out.
    ///
    /// Returns `Some(text)` on confirm, `None` on back. The `Comm` argument is
    /// kept for API parity with other NBGL wrappers; this implementation pumps
    /// SE events through `seph::io_rx` directly so any APDU that arrives while
    /// the keyboard is on screen will be dropped — fine for the demo flow that
    /// runs once at startup, before the APDU loop.
    pub fn show<const N: usize>(self, _comm: &mut Comm<N>) -> Option<String> {
        // Reset state.
        KEYBOARD_DONE.store(false, Ordering::Release);
        KEYBOARD_CONFIRMED.store(false, Ordering::Release);
        // SAFETY: single-threaded device runtime; no concurrent access.
        unsafe {
            for b in &mut *core::ptr::addr_of_mut!(TEXT_BUFFER) {
                *b = 0;
            }
        }

        let entry_ptr =
            core::ptr::addr_of_mut!(TEXT_BUFFER) as *mut u8 as *mut c_char;

        let params = nbgl_keyboardParams_t {
            type_: KEYBOARD_WITH_BUTTON,
            title: self.title.as_ptr(),
            entry_buffer: entry_ptr,
            entry_max_len: self.entry_max_len,
            mode: self.mode,
            letters_only: self.letters_only,
            numbered: false,
            number: 0,
            casing: self.casing,
            variant: nbgl_keyboardParams_variant {
                confirmation_params: nbgl_kbdButtonParams_t {
                    button_text: self.button_text.as_ptr(),
                    on_button_callback: Some(confirm_cb),
                },
            },
        };

        unsafe {
            nbgl_useCaseKeyboard(&params, Some(back_cb));
        }

        // Drive SE events until a callback flips KEYBOARD_DONE. `seph::io_rx`
        // only fills the buffer; it does not dispatch UI events to NBGL — we
        // do that manually for the events the keyboard cares about (touch,
        // ticker).
        let mut local_buf = [0u8; 273];
        while !KEYBOARD_DONE.load(Ordering::Acquire) {
            let r = ledger_device_sdk::sys::seph::io_rx(&mut local_buf, true);
            if r < 0 {
                break;
            }
            if r == 0 {
                continue;
            }
            let pt = local_buf[0];
            if pt != OS_IO_PACKET_TYPE_SEPH && pt != OS_IO_PACKET_TYPE_SE_EVT {
                continue;
            }
            let tag = local_buf[1] as u32;
            unsafe {
                if tag == SEPROXYHAL_TAG_FINGER_EVENT {
                    ux_process_finger_event(local_buf.as_ptr().add(1));
                } else if tag == SEPROXYHAL_TAG_TICKER_EVENT {
                    ux_process_ticker_event();
                }
            }
        }

        if !KEYBOARD_CONFIRMED.load(Ordering::Acquire) {
            return None;
        }

        // SAFETY: TEXT_BUFFER is only written by the NBGL C callback, which has
        // returned by the time KEYBOARD_DONE is observed.
        let bytes = unsafe { &*core::ptr::addr_of!(TEXT_BUFFER) };
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Some(String::from_utf8_lossy(&bytes[..len]).into_owned())
    }
}
