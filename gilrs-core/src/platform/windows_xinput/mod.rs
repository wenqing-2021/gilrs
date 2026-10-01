// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
mod dinput;
mod dinput_state;
mod ff;
mod gamepad;

pub use self::ff::Device as FfDevice;
pub use self::gamepad::{native_ev_codes, EvCode, Gamepad, Gilrs};

// DirectInput and SDL mappings use down-positive Y; reflect XInput to match
// the WGI, IOHID and evdev backends.
pub const IS_Y_AXIS_REVERSED: bool = true;
