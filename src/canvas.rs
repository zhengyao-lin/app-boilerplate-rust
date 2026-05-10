//! A thin wrapper over NBGL's low-level object-tree primitives.
//!
//! Builds a blank screen with arbitrary `TEXT_AREA` and `BUTTON` objects placed
//! at absolute pixel positions, installs it via `nbgl_screenSet`, drives SE
//! events itself (same pattern as `crate::keyboard` / `crate::nav`), and
//! returns the token of whichever button the user tapped.
//!
//! Intended as a demo of the underlying drawing primitives. The objects come
//! from the OS-managed `nbgl_objPoolGet` pool, so they live for the lifetime
//! of the screen and are reclaimed when the next `nbgl_screenSet` runs.

extern crate alloc;

use alloc::ffi::CString;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ledger_device_sdk::io::Comm;
use ledger_device_sdk::sys::{
    nbgl_area_t, nbgl_button_t, nbgl_containerPoolGet, nbgl_obj_t, nbgl_objPoolGet, nbgl_refresh,
    nbgl_screenRedraw, nbgl_screenSet, nbgl_text_area_t, nbgl_touchType_t,
    ux_process_finger_event, ux_process_ticker_event, BAGL_FONT_INTER_REGULAR_24px,
    BAGL_FONT_INTER_SEMIBOLD_24px, BLACK, BUTTON, CENTER, LIGHT_GRAY, NBGL_BPP_4, NO_STYLE,
    OS_IO_PACKET_TYPE_SEPH, OS_IO_PACKET_TYPE_SE_EVT, RADIUS_32_PIXELS, SEPROXYHAL_TAG_FINGER_EVENT,
    SEPROXYHAL_TAG_TICKER_EVENT, TEXT_AREA, TOP_LEFT, TOUCHED, WHITE,
};

// One layer of the screen stack used by all our objects.
const LAYER: u8 = 0;

/// Visual style of a [`Item::Button`].
#[derive(Copy, Clone)]
pub enum ButtonStyle {
    /// White inside, light-gray border, black text.
    Light,
    /// Black inside, black border, white text.
    Dark,
    /// White inside, no visible border, black text.
    NoBorder,
}

/// One placeable element on the canvas.
pub enum Item {
    /// Read-only label. `font_id` defaults to the regular 24px font.
    Text {
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        text: String,
    },
    /// Tappable rounded button. The token is reported back from `show()`.
    Button {
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        text: String,
        token: u8,
        style: ButtonStyle,
    },
}

pub struct Canvas {
    items: Vec<Item>,
}

impl Canvas {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn text(mut self, x: i16, y: i16, w: u16, h: u16, text: impl Into<String>) -> Self {
        self.items.push(Item::Text {
            x,
            y,
            w,
            h,
            text: text.into(),
        });
        self
    }

    pub fn button(
        mut self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        text: impl Into<String>,
        token: u8,
        style: ButtonStyle,
    ) -> Self {
        self.items.push(Item::Button {
            x,
            y,
            w,
            h,
            text: text.into(),
            token,
            style,
        });
        self
    }

    /// Render the canvas and block until any button is tapped. Returns the
    /// tapped button's token, or `None` on a hardware error in the event pump.
    pub fn show<const N: usize>(self, _comm: &mut Comm<N>) -> Option<u8> {
        if self.items.is_empty() {
            return None;
        }

        // Keep the CStrings alive until after `show()` returns. The button /
        // text_area objects hold raw `*const c_char` pointers into them.
        let strings: Vec<CString> = self
            .items
            .iter()
            .map(|it| match it {
                Item::Text { text, .. } | Item::Button { text, .. } => {
                    CString::new(text.as_str()).unwrap()
                }
            })
            .collect();

        TAPPED.store(false, Ordering::Release);
        TAPPED_TOKEN.store(0, Ordering::Release);

        unsafe {
            // Install a fresh screen with up to `nb_items` children. The
            // syscall fills `screen_children` with a pointer to its internal
            // pool; we then write our object pointers into that array.
            let nb_items = self.items.len() as u8;
            let mut screen_children: *mut *mut nbgl_obj_t = ptr::null_mut();
            let _rc = nbgl_screenSet(
                &mut screen_children as *mut _,
                nb_items,
                ptr::null(),
                Some(touch_cb),
            );
            // Defensive: containerPoolGet uses the same layer, only used if
            // screenSet didn't populate `screen_children` (older SDKs).
            if screen_children.is_null() {
                screen_children = nbgl_containerPoolGet(nb_items, LAYER);
            }

            for (i, (item, cstr)) in self.items.iter().zip(strings.iter()).enumerate() {
                let obj = match item {
                    Item::Text { x, y, w, h, .. } => {
                        let p = nbgl_objPoolGet(TEXT_AREA, LAYER) as *mut nbgl_text_area_t;
                        ptr::write(
                            p,
                            nbgl_text_area_t {
                                obj: base_obj(*x, *y, *w, *h, TEXT_AREA, 0, 0),
                                textColor: BLACK,
                                textAlignment: CENTER,
                                style: NO_STYLE,
                                fontId: BAGL_FONT_INTER_REGULAR_24px,
                                autoHideLongLine: false,
                                _bitfield_align_1: [],
                                _bitfield_1: Default::default(),
                                nbMaxLines: 1,
                                text: cstr.as_ptr(),
                                len: cstr.as_bytes().len() as u16,
                                onDrawCallback: None,
                                token: 0,
                                obfuscated: false,
                            },
                        );
                        p as *mut nbgl_obj_t
                    }
                    Item::Button {
                        x,
                        y,
                        w,
                        h,
                        token,
                        style,
                        ..
                    } => {
                        let (inner, border, fg) = match style {
                            ButtonStyle::Light => (WHITE, LIGHT_GRAY, BLACK),
                            ButtonStyle::Dark => (BLACK, BLACK, WHITE),
                            ButtonStyle::NoBorder => (WHITE, WHITE, BLACK),
                        };
                        let p = nbgl_objPoolGet(BUTTON, LAYER) as *mut nbgl_button_t;
                        ptr::write(
                            p,
                            nbgl_button_t {
                                obj: base_obj(*x, *y, *w, *h, BUTTON, 1u16 << TOUCHED, *token),
                                text: cstr.as_ptr(),
                                onDrawCallback: None,
                                icon: ptr::null(),
                                innerColor: inner,
                                borderColor: border,
                                foregroundColor: fg,
                                radius: RADIUS_32_PIXELS,
                                fontId: BAGL_FONT_INTER_SEMIBOLD_24px,
                                localized: false,
                                token: *token,
                            },
                        );
                        p as *mut nbgl_obj_t
                    }
                };
                *screen_children.add(i) = obj;
            }

            nbgl_screenRedraw();
            nbgl_refresh();
        }

        // Drive SE events until the touch callback fires.
        let mut buf = [0u8; 273];
        loop {
            if TAPPED.load(Ordering::Acquire) {
                return Some(TAPPED_TOKEN.load(Ordering::Acquire));
            }
            let r = ledger_device_sdk::sys::seph::io_rx(&mut buf, true);
            if r < 0 {
                return None;
            }
            if r == 0 {
                continue;
            }
            let pt = buf[0];
            if pt != OS_IO_PACKET_TYPE_SEPH && pt != OS_IO_PACKET_TYPE_SE_EVT {
                continue;
            }
            let tag = buf[1] as u32;
            unsafe {
                if tag == SEPROXYHAL_TAG_FINGER_EVENT {
                    ux_process_finger_event(buf.as_ptr().add(1));
                } else if tag == SEPROXYHAL_TAG_TICKER_EVENT {
                    ux_process_ticker_event();
                }
            }
        }
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

// Shared fields of every nbgl_obj_t. We set `alignment = TOP_LEFT` and stash
// the absolute (x, y) into `alignmentMargin{X,Y}`. NBGL's `NO_ALIGNMENT` means
// "let the parent container auto-layout me" — the screen container defaults
// to VERTICAL layout, which would stack every child at the top-left. Using
// TOP_LEFT with `alignTo = NULL` aligns to the screen's top-left and treats
// the margins as absolute pixel coordinates, which is what we want.
fn base_obj(
    x0: i16,
    y0: i16,
    width: u16,
    height: u16,
    type_: ledger_device_sdk::sys::nbgl_obj_type_t,
    touch_mask: u16,
    obj_id: u8,
) -> nbgl_obj_t {
    nbgl_obj_t {
        area: nbgl_area_t {
            x0: 0,
            y0: 0,
            width,
            height,
            backgroundColor: WHITE,
            bpp: NBGL_BPP_4,
        },
        type_,
        alignment: TOP_LEFT,
        parent: ptr::null_mut(),
        alignTo: ptr::null_mut(),
        alignmentMarginX: x0,
        alignmentMarginY: y0,
        touchMask: touch_mask,
        touchId: 0,
        objId: obj_id,
    }
}

// ---- touch dispatch --------------------------------------------------------

static TAPPED: AtomicBool = AtomicBool::new(false);
static TAPPED_TOKEN: AtomicU8 = AtomicU8::new(0);

unsafe extern "C" fn touch_cb(obj: *mut c_void, event: nbgl_touchType_t) {
    if event != TOUCHED || obj.is_null() {
        return;
    }
    // We only set touchMask=(1<<TOUCHED) on buttons, so this is always a
    // button. Read its `token` field via an unaligned read because
    // `nbgl_button_s` is `#[repr(C, packed)]`.
    let btn = obj as *mut nbgl_button_t;
    let token = unsafe { ptr::read_unaligned(ptr::addr_of!((*btn).token)) };
    TAPPED_TOKEN.store(token, Ordering::Release);
    TAPPED.store(true, Ordering::Release);
}
