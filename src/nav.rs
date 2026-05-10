//! A wrapper around the asynchronous NBGL `nbgl_useCaseNavigableContent` C API.
//!
//! Mirrors the layout of `crate::keyboard`: a single-instance, single-threaded
//! builder that pushes its content into a `static`-stashed leaked `Box`, calls
//! the C use case, and pumps SE events via `seph::io_rx` until the user backs
//! out.
//!
//! All eleven `nbgl_pageContent_t` content variants are supported via the
//! `NavPage` enum. Icons and animations are intentionally omitted (set to
//! null) — the C side handles null icons gracefully.

extern crate alloc;

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use ledger_device_sdk::io::Comm;
use ledger_device_sdk::sys::{
    nbgl_contentBarsList_t, nbgl_contentCenter_t, nbgl_contentCenteredInfo_t,
    nbgl_contentExtendedCenter_t, nbgl_contentInfoButton_t, nbgl_contentInfoList_t,
    nbgl_contentInfoLongPress_t, nbgl_contentRadioChoice_t, nbgl_contentRadioChoice_t__bindgen_ty_1,
    nbgl_contentSwitch_t, nbgl_contentSwitchesList_t, nbgl_contentTagValue_t,
    nbgl_contentTagValueConfirm_t, nbgl_contentTagValueDetails_t, nbgl_contentTagValueList_t,
    nbgl_contentTipBox_t, nbgl_pageContent_t, nbgl_useCaseNavigableContent,
    ux_process_finger_event, ux_process_ticker_event, BARS_LIST, CENTERED_INFO, CHOICES_LIST,
    EXTENDED_CENTER, INFOS_LIST, INFO_BUTTON, INFO_LONG_PRESS, OFF_STATE, ON_STATE,
    OS_IO_PACKET_TYPE_SEPH, OS_IO_PACKET_TYPE_SE_EVT, SEPROXYHAL_TAG_FINGER_EVENT,
    SEPROXYHAL_TAG_TICKER_EVENT, SWITCHES_LIST, TAG_VALUE_CONFIRM, TAG_VALUE_DETAILS,
    TAG_VALUE_LIST,
};

// ---- public API ------------------------------------------------------------

pub struct Switch {
    pub text: String,
    pub sub_text: String,
    pub init_on: bool,
    pub token: u8,
}

pub struct BarItem {
    pub text: String,
    pub token: u8,
}

/// One page of navigable content, supplied by the caller.
pub enum NavPage {
    /// Three centered text lines.
    CenteredInfo {
        text1: String,
        text2: String,
        text3: String,
    },
    /// Center-block with title/small_title/description/sub_text plus an
    /// optional bottom tip box.
    ExtendedCenter {
        title: String,
        small_title: String,
        description: String,
        sub_text: String,
        tip_text: String,
    },
    /// Body text with a long-press confirmation footer.
    InfoLongPress {
        text: String,
        long_press_text: String,
        long_press_token: u8,
    },
    /// Body text with a regular confirmation button.
    InfoButton {
        text: String,
        button_text: String,
        button_token: u8,
    },
    /// `(item, value)` list.
    TagValueList { pairs: Vec<(String, String)> },
    /// `(item, value)` list plus a "details" button.
    TagValueDetails {
        pairs: Vec<(String, String)>,
        details_button_text: String,
        details_button_token: u8,
    },
    /// `(item, value)` list with both a details button and confirm/cancel
    /// actions in the footer.
    TagValueConfirm {
        pairs: Vec<(String, String)>,
        details_button_text: String,
        details_button_token: u8,
        confirmation_text: String,
        cancel_text: String,
        confirmation_token: u8,
        cancel_token: u8,
    },
    /// List of toggle switches.
    SwitchesList { switches: Vec<Switch> },
    /// List of read-only `(name, value)` info rows (used for "About" pages).
    InfosList { infos: Vec<(String, String)> },
    /// Radio-style single-choice list.
    ChoicesList {
        choices: Vec<String>,
        init_choice: u8,
        token: u8,
    },
    /// Vertical bar list (each row navigates somewhere via its token).
    BarsList { bars: Vec<BarItem> },
}

/// Builder for a multi-page navigable screen.
pub struct NbglNav {
    title: CString,
    pages: Vec<NavPage>,
}

impl NbglNav {
    pub fn new(title: &str) -> Self {
        Self {
            title: CString::new(title).unwrap(),
            pages: Vec::new(),
        }
    }

    pub fn page(mut self, page: NavPage) -> Self {
        self.pages.push(page);
        self
    }

    /// Display the pages and block until the user backs out via the title bar.
    pub fn show<const N: usize>(self, _comm: &mut Comm<N>) {
        if self.pages.is_empty() {
            return;
        }
        let nb_pages = self.pages.len() as u8;

        let prepared: Box<Vec<PreparedPage>> =
            Box::new(self.pages.into_iter().map(prepare_page).collect());

        // Install storage for the nav callback before calling the C entry
        // point. The guard frees it on scope exit.
        NAV_PAGES_PTR.store(Box::into_raw(prepared), Ordering::Release);
        let _guard = NavPagesGuard;
        NAV_DONE.store(false, Ordering::Release);

        unsafe {
            nbgl_useCaseNavigableContent(
                self.title.as_ptr(),
                0,
                nb_pages,
                Some(quit_cb),
                Some(nav_cb),
                Some(controls_cb),
            );
        }

        // Drive SE events until quit_cb fires. Same pattern as keyboard.rs:
        // `seph::io_rx` only fills the buffer; UI dispatch is on us.
        let mut local_buf = [0u8; 273];
        while !NAV_DONE.load(Ordering::Acquire) {
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
    }
}

// ---- internal storage ------------------------------------------------------
//
// Each variant of `PreparedPage` keeps the owned `CString`s + any contiguous
// FFI arrays referenced by raw pointers in the ABI struct. As long as the
// owning `Box<Vec<PreparedPage>>` is alive (until `NavPagesGuard::drop`), the
// allocations behind those pointers do not move, so the C side can safely read
// them on every `nav_cb` call.

struct CenteredInfoStore {
    _t1: CString,
    _t2: CString,
    _t3: CString,
    abi: nbgl_contentCenteredInfo_t,
}

struct ExtendedCenterStore {
    _title: CString,
    _small_title: CString,
    _description: CString,
    _sub_text: CString,
    _tip_text: CString,
    abi: nbgl_contentExtendedCenter_t,
}

struct InfoLongPressStore {
    _text: CString,
    _long_press_text: CString,
    abi: nbgl_contentInfoLongPress_t,
}

struct InfoButtonStore {
    _text: CString,
    _button_text: CString,
    abi: nbgl_contentInfoButton_t,
}

struct TagValueListStore {
    _strings: Vec<(CString, CString)>,
    abi_pairs: Vec<nbgl_contentTagValue_t>,
    abi: nbgl_contentTagValueList_t,
}

struct TagValueDetailsStore {
    _strings: Vec<(CString, CString)>,
    abi_pairs: Vec<nbgl_contentTagValue_t>,
    _details_button_text: CString,
    abi: nbgl_contentTagValueDetails_t,
}

struct TagValueConfirmStore {
    _strings: Vec<(CString, CString)>,
    abi_pairs: Vec<nbgl_contentTagValue_t>,
    _details_button_text: CString,
    _confirmation_text: CString,
    _cancel_text: CString,
    abi: nbgl_contentTagValueConfirm_t,
}

struct SwitchesListStore {
    _strings: Vec<(CString, CString)>, // (text, sub_text) per switch
    abi_switches: Vec<nbgl_contentSwitch_t>,
    abi: nbgl_contentSwitchesList_t,
}

struct InfosListStore {
    _strings: Vec<(CString, CString)>, // (type, content) per row
    abi_types: Vec<*const c_char>,
    abi_contents: Vec<*const c_char>,
    abi: nbgl_contentInfoList_t,
}

struct ChoicesListStore {
    _strings: Vec<CString>,
    abi_names: Vec<*const c_char>,
    abi: nbgl_contentRadioChoice_t,
}

struct BarsListStore {
    _strings: Vec<CString>,
    abi_texts: Vec<*const c_char>,
    abi_tokens: Vec<u8>,
    abi: nbgl_contentBarsList_t,
}

enum PreparedPage {
    CenteredInfo(CenteredInfoStore),
    ExtendedCenter(ExtendedCenterStore),
    InfoLongPress(InfoLongPressStore),
    InfoButton(InfoButtonStore),
    TagValueList(TagValueListStore),
    TagValueDetails(TagValueDetailsStore),
    TagValueConfirm(TagValueConfirmStore),
    SwitchesList(SwitchesListStore),
    InfosList(InfosListStore),
    ChoicesList(ChoicesListStore),
    BarsList(BarsListStore),
}

// Null on init so the static lands in .bss (Stax linker rejects non-empty
// .data). The pointer owns a leaked `Box<Vec<PreparedPage>>` for the duration
// of one `show()` call.
static NAV_PAGES_PTR: AtomicPtr<Vec<PreparedPage>> = AtomicPtr::new(core::ptr::null_mut());
static NAV_DONE: AtomicBool = AtomicBool::new(false);

struct NavPagesGuard;
impl Drop for NavPagesGuard {
    fn drop(&mut self) {
        let p = NAV_PAGES_PTR.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            // SAFETY: pointer was created by Box::into_raw in `show()`.
            unsafe {
                drop(Box::from_raw(p));
            }
        }
    }
}

// ---- prepare_page ---------------------------------------------------------

fn cstr(s: String) -> CString {
    CString::new(s).unwrap()
}

fn build_tag_value_pairs(
    pairs: Vec<(String, String)>,
) -> (Vec<(CString, CString)>, Vec<nbgl_contentTagValue_t>) {
    let strings: Vec<(CString, CString)> = pairs
        .into_iter()
        .map(|(k, v)| (cstr(k), cstr(v)))
        .collect();
    let abi_pairs: Vec<nbgl_contentTagValue_t> = strings
        .iter()
        .map(|(k, v)| nbgl_contentTagValue_t {
            item: k.as_ptr(),
            value: v.as_ptr(),
            ..Default::default()
        })
        .collect();
    (strings, abi_pairs)
}

fn prepare_page(page: NavPage) -> PreparedPage {
    match page {
        NavPage::CenteredInfo {
            text1,
            text2,
            text3,
        } => {
            let t1 = cstr(text1);
            let t2 = cstr(text2);
            let t3 = cstr(text3);
            let abi = nbgl_contentCenteredInfo_t {
                text1: t1.as_ptr(),
                text2: t2.as_ptr(),
                text3: t3.as_ptr(),
                ..Default::default()
            };
            PreparedPage::CenteredInfo(CenteredInfoStore {
                _t1: t1,
                _t2: t2,
                _t3: t3,
                abi,
            })
        }
        NavPage::ExtendedCenter {
            title,
            small_title,
            description,
            sub_text,
            tip_text,
        } => {
            let title = cstr(title);
            let small_title = cstr(small_title);
            let description = cstr(description);
            let sub_text = cstr(sub_text);
            let tip_text = cstr(tip_text);
            let abi = nbgl_contentExtendedCenter_t {
                contentCenter: nbgl_contentCenter_t {
                    title: title.as_ptr(),
                    smallTitle: small_title.as_ptr(),
                    description: description.as_ptr(),
                    subText: sub_text.as_ptr(),
                    ..Default::default()
                },
                tipBox: nbgl_contentTipBox_t {
                    text: tip_text.as_ptr(),
                    ..Default::default()
                },
            };
            PreparedPage::ExtendedCenter(ExtendedCenterStore {
                _title: title,
                _small_title: small_title,
                _description: description,
                _sub_text: sub_text,
                _tip_text: tip_text,
                abi,
            })
        }
        NavPage::InfoLongPress {
            text,
            long_press_text,
            long_press_token,
        } => {
            let text = cstr(text);
            let long_press_text = cstr(long_press_text);
            let abi = nbgl_contentInfoLongPress_t {
                text: text.as_ptr(),
                longPressText: long_press_text.as_ptr(),
                longPressToken: long_press_token,
                ..Default::default()
            };
            PreparedPage::InfoLongPress(InfoLongPressStore {
                _text: text,
                _long_press_text: long_press_text,
                abi,
            })
        }
        NavPage::InfoButton {
            text,
            button_text,
            button_token,
        } => {
            let text = cstr(text);
            let button_text = cstr(button_text);
            let abi = nbgl_contentInfoButton_t {
                text: text.as_ptr(),
                buttonText: button_text.as_ptr(),
                buttonToken: button_token,
                ..Default::default()
            };
            PreparedPage::InfoButton(InfoButtonStore {
                _text: text,
                _button_text: button_text,
                abi,
            })
        }
        NavPage::TagValueList { pairs } => {
            let (strings, abi_pairs) = build_tag_value_pairs(pairs);
            let abi = nbgl_contentTagValueList_t {
                pairs: abi_pairs.as_ptr(),
                nbPairs: abi_pairs.len() as u8,
                ..Default::default()
            };
            PreparedPage::TagValueList(TagValueListStore {
                _strings: strings,
                abi_pairs,
                abi,
            })
        }
        NavPage::TagValueDetails {
            pairs,
            details_button_text,
            details_button_token,
        } => {
            let (strings, abi_pairs) = build_tag_value_pairs(pairs);
            let details_button_text = cstr(details_button_text);
            let abi = nbgl_contentTagValueDetails_t {
                tagValueList: nbgl_contentTagValueList_t {
                    pairs: abi_pairs.as_ptr(),
                    nbPairs: abi_pairs.len() as u8,
                    ..Default::default()
                },
                detailsButtonText: details_button_text.as_ptr(),
                detailsButtonToken: details_button_token,
                ..Default::default()
            };
            PreparedPage::TagValueDetails(TagValueDetailsStore {
                _strings: strings,
                abi_pairs,
                _details_button_text: details_button_text,
                abi,
            })
        }
        NavPage::TagValueConfirm {
            pairs,
            details_button_text,
            details_button_token,
            confirmation_text,
            cancel_text,
            confirmation_token,
            cancel_token,
        } => {
            let (strings, abi_pairs) = build_tag_value_pairs(pairs);
            let details_button_text = cstr(details_button_text);
            let confirmation_text = cstr(confirmation_text);
            let cancel_text = cstr(cancel_text);
            let abi = nbgl_contentTagValueConfirm_t {
                tagValueList: nbgl_contentTagValueList_t {
                    pairs: abi_pairs.as_ptr(),
                    nbPairs: abi_pairs.len() as u8,
                    ..Default::default()
                },
                detailsButtonText: details_button_text.as_ptr(),
                detailsButtonToken: details_button_token,
                confirmationText: confirmation_text.as_ptr(),
                cancelText: cancel_text.as_ptr(),
                confirmationToken: confirmation_token,
                cancelToken: cancel_token,
                ..Default::default()
            };
            PreparedPage::TagValueConfirm(TagValueConfirmStore {
                _strings: strings,
                abi_pairs,
                _details_button_text: details_button_text,
                _confirmation_text: confirmation_text,
                _cancel_text: cancel_text,
                abi,
            })
        }
        NavPage::SwitchesList { switches } => {
            let strings: Vec<(CString, CString)> = switches
                .iter()
                .map(|s| (cstr(s.text.clone()), cstr(s.sub_text.clone())))
                .collect();
            let abi_switches: Vec<nbgl_contentSwitch_t> = switches
                .iter()
                .zip(strings.iter())
                .map(|(sw, (text, sub_text))| nbgl_contentSwitch_t {
                    text: text.as_ptr(),
                    subText: sub_text.as_ptr(),
                    initState: if sw.init_on { ON_STATE } else { OFF_STATE },
                    token: sw.token,
                    ..Default::default()
                })
                .collect();
            let abi = nbgl_contentSwitchesList_t {
                switches: abi_switches.as_ptr(),
                nbSwitches: abi_switches.len() as u8,
            };
            PreparedPage::SwitchesList(SwitchesListStore {
                _strings: strings,
                abi_switches,
                abi,
            })
        }
        NavPage::InfosList { infos } => {
            let strings: Vec<(CString, CString)> = infos
                .into_iter()
                .map(|(t, c)| (cstr(t), cstr(c)))
                .collect();
            let abi_types: Vec<*const c_char> = strings.iter().map(|(t, _)| t.as_ptr()).collect();
            let abi_contents: Vec<*const c_char> =
                strings.iter().map(|(_, c)| c.as_ptr()).collect();
            let abi = nbgl_contentInfoList_t {
                infoTypes: abi_types.as_ptr(),
                infoContents: abi_contents.as_ptr(),
                nbInfos: strings.len() as u8,
                ..Default::default()
            };
            PreparedPage::InfosList(InfosListStore {
                _strings: strings,
                abi_types,
                abi_contents,
                abi,
            })
        }
        NavPage::ChoicesList {
            choices,
            init_choice,
            token,
        } => {
            let strings: Vec<CString> = choices.into_iter().map(cstr).collect();
            let abi_names: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
            let abi = nbgl_contentRadioChoice_t {
                __bindgen_anon_1: nbgl_contentRadioChoice_t__bindgen_ty_1 {
                    names: abi_names.as_ptr(),
                },
                localized: false,
                nbChoices: strings.len() as u8,
                initChoice: init_choice,
                token,
                ..Default::default()
            };
            PreparedPage::ChoicesList(ChoicesListStore {
                _strings: strings,
                abi_names,
                abi,
            })
        }
        NavPage::BarsList { bars } => {
            let strings: Vec<CString> = bars.iter().map(|b| cstr(b.text.clone())).collect();
            let abi_texts: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
            let abi_tokens: Vec<u8> = bars.iter().map(|b| b.token).collect();
            let abi = nbgl_contentBarsList_t {
                barTexts: abi_texts.as_ptr(),
                tokens: abi_tokens.as_ptr(),
                nbBars: bars.len() as u8,
                ..Default::default()
            };
            PreparedPage::BarsList(BarsListStore {
                _strings: strings,
                abi_texts,
                abi_tokens,
                abi,
            })
        }
    }
}

// ---- C callbacks -----------------------------------------------------------

unsafe extern "C" fn nav_cb(page: u8, content: *mut nbgl_pageContent_t) -> bool {
    let ptr = NAV_PAGES_PTR.load(Ordering::Acquire);
    if ptr.is_null() {
        return false;
    }
    // SAFETY: ptr is a valid `Box<Vec<PreparedPage>>` allocation while
    // NAV_PAGES_PTR is non-null (cleared by NavPagesGuard on scope exit, after
    // the C use case has returned).
    let pages: &Vec<PreparedPage> = unsafe { &*ptr };
    let Some(prepared) = pages.get(page as usize) else {
        return false;
    };
    unsafe {
        let u = &mut (*content).__bindgen_anon_1;
        match prepared {
            PreparedPage::CenteredInfo(p) => {
                (*content).type_ = CENTERED_INFO;
                u.centeredInfo = p.abi;
            }
            PreparedPage::ExtendedCenter(p) => {
                (*content).type_ = EXTENDED_CENTER;
                u.extendedCenter = p.abi;
            }
            PreparedPage::InfoLongPress(p) => {
                (*content).type_ = INFO_LONG_PRESS;
                u.infoLongPress = p.abi;
            }
            PreparedPage::InfoButton(p) => {
                (*content).type_ = INFO_BUTTON;
                u.infoButton = p.abi;
            }
            PreparedPage::TagValueList(p) => {
                (*content).type_ = TAG_VALUE_LIST;
                let mut tvl = p.abi;
                tvl.pairs = p.abi_pairs.as_ptr();
                tvl.nbPairs = p.abi_pairs.len() as u8;
                u.tagValueList = tvl;
            }
            PreparedPage::TagValueDetails(p) => {
                (*content).type_ = TAG_VALUE_DETAILS;
                let mut details = p.abi;
                details.tagValueList.pairs = p.abi_pairs.as_ptr();
                details.tagValueList.nbPairs = p.abi_pairs.len() as u8;
                u.tagValueDetails = details;
            }
            PreparedPage::TagValueConfirm(p) => {
                (*content).type_ = TAG_VALUE_CONFIRM;
                let mut conf = p.abi;
                conf.tagValueList.pairs = p.abi_pairs.as_ptr();
                conf.tagValueList.nbPairs = p.abi_pairs.len() as u8;
                u.tagValueConfirm = conf;
            }
            PreparedPage::SwitchesList(p) => {
                (*content).type_ = SWITCHES_LIST;
                let mut sw = p.abi;
                sw.switches = p.abi_switches.as_ptr();
                sw.nbSwitches = p.abi_switches.len() as u8;
                u.switchesList = sw;
            }
            PreparedPage::InfosList(p) => {
                (*content).type_ = INFOS_LIST;
                let mut il = p.abi;
                il.infoTypes = p.abi_types.as_ptr();
                il.infoContents = p.abi_contents.as_ptr();
                il.nbInfos = p.abi_types.len() as u8;
                u.infosList = il;
            }
            PreparedPage::ChoicesList(p) => {
                (*content).type_ = CHOICES_LIST;
                let mut cl = p.abi;
                cl.__bindgen_anon_1.names = p.abi_names.as_ptr();
                cl.nbChoices = p.abi_names.len() as u8;
                u.choicesList = cl;
            }
            PreparedPage::BarsList(p) => {
                (*content).type_ = BARS_LIST;
                let mut bl = p.abi;
                bl.barTexts = p.abi_texts.as_ptr();
                bl.tokens = p.abi_tokens.as_ptr();
                bl.nbBars = p.abi_texts.len() as u8;
                u.barsList = bl;
            }
        }
    }
    true
}

unsafe extern "C" fn quit_cb() {
    NAV_DONE.store(true, Ordering::Release);
}

unsafe extern "C" fn controls_cb(_token: c_int, _index: u8) {
    // No interactive controls handled here. Switch toggles, button presses
    // and choice selections all flow through this callback in the underlying
    // C use case; expose them later if/when we need to react to them.
}
