/*****************************************************************************
 *   Ledger App Boilerplate Rust.
 *   (c) 2023 Ledger SAS.
 *
 *  Licensed under the Apache License, Version 2.0 (the "License");
 *  you may not use this file except in compliance with the License.
 *  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *  Unless required by applicable law or agreed to in writing, software
 *  distributed under the License is distributed on an "AS IS" BASIS,
 *  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  See the License for the specific language governing permissions and
 *  limitations under the License.
 *****************************************************************************/

#![no_std]
#![no_main]

mod utils;
mod app_ui {
    pub mod address;
    pub mod menu;
    pub mod sign;
}
mod handlers {
    pub mod get_public_key;
    pub mod get_version;
    pub mod sign_tx;
}

mod keyboard;
mod nav;
mod settings;
mod swap;

use app_ui::menu::ui_menu_main;
use handlers::{
    get_public_key::handler_get_public_key,
    get_version::handler_get_version,
    sign_tx::{handler_sign_tx, TxContext},
};
use ledger_device_sdk::io::{self, init_comm, ApduHeader, Comm, Command, Reply, StatusWords};
use ledger_device_sdk::libcall::swap::CreateTxParams;
use ledger_device_sdk::screen::*;
use ledger_device_sdk::*;

ledger_device_sdk::set_panic!(ledger_device_sdk::exiting_panic);

// Required for using String, Vec, format!...
extern crate alloc;

use ledger_device_sdk::nbgl::*;

ledger_device_sdk::define_comm!(COMM);

// P2 for last APDU to receive.
const P2_SIGN_TX_LAST: u8 = 0x00;
// P2 for more APDU to receive.
const P2_SIGN_TX_MORE: u8 = 0x80;
// P1 for first APDU number.
const P1_SIGN_TX_START: u8 = 0x00;
// P1 for maximum APDU number.
const P1_SIGN_TX_MAX: u8 = 0x03;

// Application status words.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq)]
pub enum AppSW {
    Deny = 0x6985,
    WrongP1P2 = 0x6A86,
    InsNotSupported = 0x6D00,
    ClaNotSupported = 0x6E00,
    CommError = 0x6F00,
    TxDisplayFail = 0xB001,
    AddrDisplayFail = 0xB002,
    TxWrongLength = 0xB004,
    TxParsingFail = 0xB005,
    TxHashFail = 0xB006,
    TxSignFail = 0xB008,
    KeyDeriveFail = 0xB009,
    VersionParsingFail = 0xB00A,
    WrongApduLength = StatusWords::BadLen as u16,
    SwapFail = 0xC000,
    Ok = 0x9000,
}

impl From<AppSW> for Reply {
    fn from(sw: AppSW) -> Reply {
        Reply(sw as u16)
    }
}

impl From<io::CommError> for AppSW {
    fn from(_e: io::CommError) -> Self {
        AppSW::CommError
    }
}

/// Possible input commands received through APDUs.
#[derive(Debug)]
pub enum Instruction {
    GetVersion,
    GetAppName,
    GetPubkey { display: bool },
    SignTx { chunk: u8, more: bool },
}

impl TryFrom<ApduHeader> for Instruction {
    type Error = AppSW;

    /// APDU parsing logic.
    ///
    /// Parses INS, P1 and P2 bytes to build an [`Instruction`]. P1 and P2 are translated to
    /// strongly typed variables depending on the APDU instruction code. Invalid INS, P1 or P2
    /// values result in errors with a status word, which are automatically sent to the host by the
    /// SDK.
    ///
    /// This design allows a clear separation of the APDU parsing logic and commands handling.
    ///
    /// Note that CLA is not checked here. Instead the method [`Comm::set_expected_cla`] is used in
    /// [`sample_main`] to have this verification automatically performed by the SDK.
    fn try_from(value: ApduHeader) -> Result<Self, Self::Error> {
        match (value.ins, value.p1, value.p2) {
            (3, 0, 0) => Ok(Instruction::GetVersion),
            (4, 0, 0) => Ok(Instruction::GetAppName),
            (5, 0 | 1, 0) => Ok(Instruction::GetPubkey {
                display: value.p1 != 0,
            }),
            (6, P1_SIGN_TX_START, P2_SIGN_TX_MORE)
            | (6, 1..=P1_SIGN_TX_MAX, P2_SIGN_TX_LAST | P2_SIGN_TX_MORE) => {
                Ok(Instruction::SignTx {
                    chunk: value.p1,
                    more: value.p2 == P2_SIGN_TX_MORE,
                })
            }
            (3..=6, _, _) => Err(AppSW::WrongP1P2),
            (_, _, _) => Err(AppSW::InsNotSupported),
        }
    }
}

fn show_status_and_home_if_needed(
    comm: &mut Comm,
    ins: &Instruction,
    tx_ctx: &mut TxContext,
    status: &AppSW,
) {
    if tx_ctx.swap_params.is_some() {
        return;
    }
    let (show_status, status_type) = match (ins, status) {
        (Instruction::GetPubkey { display: true }, AppSW::Deny | AppSW::Ok) => {
            (true, StatusType::Address)
        }
        (Instruction::SignTx { .. }, AppSW::Deny | AppSW::Ok) if tx_ctx.finished() => {
            (true, StatusType::Transaction)
        }
        (_, _) => (false, StatusType::Transaction),
    };

    if show_status {
        let success = *status == AppSW::Ok;
        NbglReviewStatus::new()
            .status_type(status_type)
            .show(comm, success);

        // call home.show_and_return() to show home and setting screen
        tx_ctx.home.show_and_return();
    }
}

// --8<-- [start:sample_main]
#[no_mangle]
extern "C" fn sample_main(arg0: u32) {
    if arg0 != 0 {
        // We have been started by the Exchange application through the os_lib_call API
        // We need to answer the command instead of starting the normal app main loop
        swap::swap_main(arg0);
    } else {
        // Normal app mode, start the main loop listening for APDU commands
        normal_main(None);
    }
}
// --8<-- [end:sample_main]

/// Main application entry point.
///
/// Handles both standard execution (user opens app) and library mode execution
/// (Exchange app calls this app for swap).
///
/// # Arguments
///
/// * `swap_params` - Optional swap parameters. If present, the app runs in "swap mode":
///   - UI is bypassed (no main menu, no transaction review)
///   - Transaction is validated against swap params
///   - Returns `true` if signed successfully, `false` otherwise
pub fn normal_main(swap_params: Option<&CreateTxParams>) -> bool {
    // Create the communication manager, and configure it to accept only APDU from the 0xe0 class.
    // If any APDU with a wrong class value is received, comm will respond automatically with
    // BadCla status word.
    let comm = init_comm(&COMM);
    comm.set_expected_cla(0xe0);

    let mut tx_ctx = if let Some(params) = swap_params {
        TxContext::new_with_swap(params)
    } else {
        TxContext::new()
    };

    if swap_params.is_none() {
        // NbglKeypad::new().title("say something").ask(comm, &[1, 2, 3, 4]);
        NbglAction::new().message("ready to go?").action_text("yes").show(comm);

        if let Some(text) = keyboard::NbglKeyboard::new()
            .title("Type something")
            .button_text("Confirm")
            .entry_max_len(32)
            .show(comm)
        {
            log!("KEYBOARD", "got: {}", text.as_str());
        }

        nav::NbglNav::new("Nav demo")
            .page(nav::NavPage::CenteredInfo {
                text1: "CenteredInfo".into(),
                text2: "Three centered text lines".into(),
                text3: "Swipe \u{2192}".into(),
            })
            .page(nav::NavPage::ExtendedCenter {
                title: "ExtendedCenter".into(),
                small_title: "Subtitle".into(),
                description: "Title plus description plus tip box.".into(),
                sub_text: "Sub text".into(),
                tip_text: "Tip box at bottom".into(),
            })
            .page(nav::NavPage::InfoLongPress {
                text: "InfoLongPress: hold the button to confirm.".into(),
                long_press_text: "Hold to confirm".into(),
                long_press_token: 0x10,
            })
            .page(nav::NavPage::InfoButton {
                text: "InfoButton: a regular tappable button.".into(),
                button_text: "Press me".into(),
                button_token: 0x11,
            })
            .page(nav::NavPage::TagValueList {
                pairs: alloc::vec![
                    ("Network".into(), "Mainnet".into()),
                    ("Account".into(), "Acct #0".into()),
                    ("Currency".into(), "BOIL".into()),
                ],
            })
            .page(nav::NavPage::TagValueDetails {
                pairs: alloc::vec![
                    ("From".into(), "0xabcd\u{2026}".into()),
                    ("Amount".into(), "1.5 BOIL".into()),
                ],
                details_button_text: "See details".into(),
                details_button_token: 0x12,
            })
            .page(nav::NavPage::TagValueConfirm {
                pairs: alloc::vec![
                    ("Action".into(), "Send".into()),
                    ("Fee".into(), "0.0001 BOIL".into()),
                ],
                details_button_text: "Details".into(),
                details_button_token: 0x13,
                confirmation_text: "Approve".into(),
                cancel_text: "Reject".into(),
                confirmation_token: 0x14,
                cancel_token: 0x15,
            })
            .page(nav::NavPage::SwitchesList {
                switches: alloc::vec![
                    nav::Switch {
                        text: "Notifications".into(),
                        sub_text: "Alerts when new tx".into(),
                        init_on: true,
                        token: 0x20,
                    },
                    nav::Switch {
                        text: "Dark mode".into(),
                        sub_text: "Use the dark theme".into(),
                        init_on: false,
                        token: 0x21,
                    },
                ],
            })
            .page(nav::NavPage::InfosList {
                infos: alloc::vec![
                    ("Version".into(), "1.8.0".into()),
                    ("Developer".into(), "Ledger".into()),
                    ("License".into(), "Apache-2.0".into()),
                ],
            })
            .page(nav::NavPage::ChoicesList {
                choices: alloc::vec!["Mainnet".into(), "Testnet".into(), "Devnet".into()],
                init_choice: 0,
                token: 0x30,
            })
            .page(nav::NavPage::BarsList {
                bars: alloc::vec![
                    nav::BarItem { text: "About".into(), token: 0x40 },
                    nav::BarItem { text: "Settings".into(), token: 0x41 },
                    nav::BarItem { text: "Reset".into(), token: 0x42 },
                ],
            })
            .page(nav::NavPage::CenteredInfo {
                text1: "End of demo".into(),
                text2: "Press back to exit".into(),
                text3: "".into(),
            })
            .show(comm);

        tx_ctx.home = ui_menu_main(comm);
        tx_ctx.home.show_and_return();

        log!("TEST", "hi");
    }

    loop {
        let command = comm.next_command();
        let decoded = command.decode::<Instruction>();
        let Ok(ins) = decoded else {
            let _ = comm.send(&[], decoded.unwrap_err());
            continue;
        };

        let _status = match handle_apdu(command, &ins, &mut tx_ctx) {
            Ok(reply) => {
                let _ = reply.send(AppSW::Ok);
                AppSW::Ok
            }
            Err(sw) => {
                let _ = comm.send(&[], sw);
                sw
            }
        };
        show_status_and_home_if_needed(comm, &ins, &mut tx_ctx, &_status);

        // In swap mode, exit after transaction is finished (signed or rejected)
        if tx_ctx.swap_params.is_some() && tx_ctx.finished() {
            return _status == AppSW::Ok;
        }
    }
}

fn handle_apdu<'a>(
    command: Command<'a>,
    ins: &Instruction,
    ctx: &mut TxContext,
) -> Result<io::CommandResponse<'a>, AppSW> {
    match ins {
        Instruction::GetAppName => {
            let mut response = command.into_response();
            response.append(env!("CARGO_PKG_NAME").as_bytes())?;
            Ok(response)
        }
        Instruction::GetVersion => handler_get_version(command),
        Instruction::GetPubkey { display } => handler_get_public_key(command, *display),
        Instruction::SignTx { chunk, more } => handler_sign_tx(command, *chunk, *more, ctx),
    }
}
