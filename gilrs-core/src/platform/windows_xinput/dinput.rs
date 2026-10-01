//! Background, nonexclusive DirectInput supplement for non-XInput controllers.
//!
//! All COM objects and the hidden cooperative-level window are created, used and
//! destroyed on the existing polling worker. Only owned metadata crosses threads.
use super::dinput_state::{is_xinput_path, mapping_uuid, pov, AXIS_BASE, BUTTON_BASE};
use super::gamepad::{native_ev_codes as nec, EvCode};
use crate::event_queue::EventSender;
use crate::{AxisInfo, Event, EventType};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows::core::{w, Interface, GUID, HRESULT};
use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
    WINDOW_EX_STYLE, WS_POPUP,
};

const MAX_DEVICES: usize = 64;
const DATA_BYTES: usize = 176; // 8 LONG axes, 4 DWORD POVs, 128 BYTE buttons
const BUFFER_EVENTS: usize = 256;
const ENUM_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_millis(250);
const FIRST_ID: usize = 4;

pub(super) type Registry = Arc<Mutex<Vec<Metadata>>>;

#[derive(Clone, Debug)]
pub(super) struct Metadata {
    pub name: String,
    pub uuid: Uuid,
    pub vendor: u16,
    pub product: u16,
    pub buttons: Vec<EvCode>,
    pub axes: Vec<EvCode>,
    infos: Vec<AxisInfo>,
}

impl Metadata {
    pub fn axis_info(&self, code: EvCode) -> Option<&AxisInfo> {
        self.axes
            .iter()
            .position(|c| *c == code)
            .map(|i| &self.infos[i])
    }
}

// Field order releases devices before DirectInput, then the window, then COM.
pub(super) struct DirectInput {
    slots: Vec<Slot>,
    input: IDirectInput8W,
    window: Window,
    _com: Com,
    registry: Registry,
    next_enum: Instant,
}

struct Slot {
    instance: GUID,
    device: Option<Device>,
}

struct Window(HWND);
struct Com;

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: this worker owns the window and all devices have been dropped.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balanced with successful initialization on this same worker.
        unsafe {
            CoUninitialize();
        }
    }
}

impl DirectInput {
    pub fn new(registry: Registry) -> windows::core::Result<Self> {
        // SAFETY: called once on a fresh dedicated worker; no interfaces escape it.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        let com = Com;
        // SAFETY: use the current process module and the system STATIC class.
        // No WS_VISIBLE, activation, message-only parent or user callback is used.
        let (window, input) = unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let window = Window(CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("gilrs background input"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )?);
            let mut raw = std::ptr::null_mut();
            DirectInput8Create(instance, 0x0800, &IDirectInput8W::IID, &mut raw, None)?;
            (window, IDirectInput8W::from_raw(raw))
        };
        Ok(Self {
            slots: Vec::new(),
            input,
            window,
            _com: com,
            registry,
            next_enum: Instant::now(),
        })
    }

    pub fn reset(&mut self) {
        for slot in &mut self.slots {
            if let Some(device) = &mut slot.device {
                device.reset();
            }
        }
        self.next_enum = Instant::now();
    }

    pub fn tick(&mut self, tx: &EventSender<Event>, epoch: u64, force: bool) {
        // SAFETY: pump only our window on its owning worker, with a bounded batch.
        unsafe {
            let mut message = MSG::default();
            for _ in 0..64 {
                if !PeekMessageW(&mut message, Some(self.window.0), 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= self.next_enum {
            self.enumerate(tx, epoch);
            self.next_enum = Instant::now() + ENUM_INTERVAL;
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(device) = &mut slot.device {
                device.tick(FIRST_ID + index, tx, epoch, force);
            }
        }
    }

    fn enumerate(&mut self, tx: &EventSender<Event>, epoch: u64) {
        let mut found = Enumeration {
            devices: [DIDEVICEINSTANCEW::default(); MAX_DEVICES],
            len: 0,
        };
        // SAFETY: synchronous callback, stack context stays live, callback does not allocate or panic.
        let result = unsafe {
            self.input.EnumDevices(
                DI8DEVCLASS_GAMECTRL,
                Some(enumerate_device),
                &mut found as *mut Enumeration as *mut c_void,
                DIEDFL_ATTACHEDONLY,
            )
        };
        if result.is_err() {
            return;
        } // transient enumeration failure is not a mass disconnect
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if !found.devices[..found.len]
                .iter()
                .any(|d| d.guidInstance == slot.instance)
            {
                if let Some(mut device) = slot.device.take() {
                    device.disconnect(FIRST_ID + index, tx, epoch);
                }
            }
        }
        for instance in &found.devices[..found.len] {
            let existing = self
                .slots
                .iter()
                .position(|s| s.instance == instance.guidInstance);
            if existing.is_some_and(|index| self.slots[index].device.is_some()) {
                continue;
            }
            if existing.is_none() && self.slots.len() == MAX_DEVICES {
                continue;
            }
            match Device::open(&self.input, self.window.0, instance) {
                Ok(Some((device, metadata))) => {
                    let index = existing.unwrap_or(self.slots.len());
                    // Publish metadata before Connected enters the ordered event queue.
                    let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                    if index == self.slots.len() {
                        registry.push(metadata);
                        self.slots.push(Slot {
                            instance: instance.guidInstance,
                            device: Some(device),
                        });
                    } else {
                        registry[index] = metadata;
                        self.slots[index].device = Some(device);
                    }
                }
                Ok(None) => {} // this is an XInput interface, serviced by the other path
                Err(error) => debug!("DirectInput open deferred: {:?}", error.code()),
            }
        }
    }
}

struct Enumeration {
    devices: [DIDEVICEINSTANCEW; MAX_DEVICES],
    len: usize,
}

unsafe extern "system" fn enumerate_device(
    info: *mut DIDEVICEINSTANCEW,
    context: *mut c_void,
) -> windows::core::BOOL {
    // SAFETY: pointers are supplied by EnumDevices and our live Enumeration context.
    // The fixed-capacity buffer avoids allocation and panics across the ABI.
    unsafe {
        let found = &mut *context.cast::<Enumeration>();
        if found.len < MAX_DEVICES {
            found.devices[found.len] = *info;
            found.len += 1;
        }
        (found.len < MAX_DEVICES).into()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Axis(EvCode),
    Button(EvCode),
    Hat(EvCode, EvCode),
}
struct Control {
    offset: usize,
    kind: Kind,
}

struct Device {
    input: IDirectInputDevice8W,
    controls: Vec<Control>,
    connected: bool,
    needs_snapshot: bool,
    retry_at: Instant,
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: device is still live, on its owner worker, before COM teardown.
        unsafe {
            let _ = self.input.Unacquire();
        }
    }
}

impl Device {
    fn open(
        input: &IDirectInput8W,
        window: HWND,
        instance: &DIDEVICEINSTANCEW,
    ) -> windows::core::Result<Option<(Self, Metadata)>> {
        let mut device = None;
        // SAFETY: live parent and valid output Option; no aggregation requested.
        unsafe {
            input.CreateDevice(&instance.guidInstance, &mut device, None)?;
        }
        let input = device
            .ok_or_else(|| windows::core::Error::from_hresult(HRESULT(0x80004005u32 as i32)))?;
        let mut path = DIPROPGUIDANDPATH {
            diph: header::<DIPROPGUIDANDPATH>(0, DIPH_DEVICE),
            ..Default::default()
        };
        // SAFETY: the property header prefixes the full initialized output structure.
        // Predefined DirectInput properties use MAKE_DIPROP's integer pseudo-pointer,
        // not the address of windows-rs's GUID-shaped DIPROP_* constants.
        unsafe {
            input.GetProperty(property(12), &mut path.diph)?;
        }
        if is_xinput_path(&path.wszPath) {
            return Ok(None);
        }
        let mut vidpid = DIPROPDWORD {
            diph: header::<DIPROPDWORD>(0, DIPH_DEVICE),
            dwData: 0,
        };
        // SAFETY: full DIPROPDWORD backing storage and device-scope property.
        unsafe {
            input.GetProperty(property(24), &mut vidpid.diph)?;
        }
        let vendor = vidpid.dwData as u16;
        let product = (vidpid.dwData >> 16) as u16;
        let mut objects = data_format_objects();
        let mut format = DIDATAFORMAT {
            dwSize: size_of::<DIDATAFORMAT>() as u32,
            dwObjSize: size_of::<DIOBJECTDATAFORMAT>() as u32,
            dwFlags: DIDF_ABSAXIS,
            dwDataSize: DATA_BYTES as u32,
            dwNumObjs: objects.len() as u32,
            rgodf: objects.as_mut_ptr(),
        };
        // SAFETY: format/object arrays and constant GUIDs live through SetDataFormat,
        // which copies them. The hidden top-level window outlives every device.
        unsafe {
            input.SetCooperativeLevel(window, DISCL_BACKGROUND | DISCL_NONEXCLUSIVE)?;
            input.SetDataFormat(&mut format)?;
        }
        let end = instance
            .tszProductName
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(instance.tszProductName.len());
        let mut metadata = Metadata {
            name: String::from_utf16_lossy(&instance.tszProductName[..end]),
            uuid: Uuid::from_bytes(mapping_uuid(vendor, product)),
            vendor,
            product,
            buttons: Vec::new(),
            axes: Vec::new(),
            infos: Vec::new(),
        };
        let mut controls = Vec::new();
        for object in &objects {
            let mut info = DIDEVICEOBJECTINSTANCEW {
                dwSize: size_of::<DIDEVICEOBJECTINSTANCEW>() as u32,
                ..Default::default()
            };
            // SAFETY: initialized output struct; query optional controls in our data format.
            if unsafe { input.GetObjectInfo(&mut info, object.dwOfs, DIPH_BYOFFSET) }.is_err() {
                continue;
            }
            let offset = object.dwOfs as usize;
            let kind = if offset < 32 {
                let mut range = DIPROPRANGE {
                    diph: header::<DIPROPRANGE>(object.dwOfs, DIPH_BYOFFSET),
                    lMin: -32768,
                    lMax: 32767,
                };
                let mut deadzone = DIPROPDWORD {
                    diph: header::<DIPROPDWORD>(object.dwOfs, DIPH_BYOFFSET),
                    dwData: 0,
                };
                // SAFETY: property structures are complete and refer to a known axis.
                unsafe {
                    // Read-only ranges remain usable with their actual bounds.
                    let _ = input.SetProperty(property(4), &mut range.diph);
                    input.GetProperty(property(4), &mut range.diph)?;
                    let _ = input.SetProperty(property(5), &mut deadzone.diph);
                }
                if range.lMin >= range.lMax {
                    return Err(windows::core::Error::from_hresult(HRESULT(
                        0x80070057u32 as i32,
                    )));
                }
                let code = EvCode(AXIS_BASE + metadata.axes.len() as u32);
                metadata.axes.push(code);
                metadata.infos.push(AxisInfo {
                    min: range.lMin,
                    max: range.lMax,
                    deadzone: None,
                });
                Kind::Axis(code)
            } else if offset < 48 {
                let hat = (offset - 32) / 4;
                let (x, y) = if hat == 0 {
                    (nec::AXIS_DPADX, nec::AXIS_DPADY)
                } else {
                    (
                        EvCode(AXIS_BASE + 16 + (hat * 2) as u32),
                        EvCode(AXIS_BASE + 17 + (hat * 2) as u32),
                    )
                };
                metadata.axes.extend([x, y]);
                metadata.infos.extend(
                    [AxisInfo {
                        min: -1,
                        max: 1,
                        deadzone: None,
                    }; 2],
                );
                Kind::Hat(x, y)
            } else {
                let code = EvCode(BUTTON_BASE + metadata.buttons.len() as u32);
                metadata.buttons.push(code);
                Kind::Button(code)
            };
            controls.push(Control { offset, kind });
        }
        let mut buffer = DIPROPDWORD {
            diph: header::<DIPROPDWORD>(0, DIPH_DEVICE),
            dwData: BUFFER_EVENTS as u32,
        };
        // SAFETY: buffering configured before acquisition, using complete property storage.
        unsafe {
            input.SetProperty(property(1), &mut buffer.diph)?;
        }
        Ok(Some((
            Self {
                input,
                controls,
                connected: false,
                needs_snapshot: true,
                retry_at: Instant::now(),
            },
            metadata,
        )))
    }

    fn reset(&mut self) {
        // SAFETY: unacquisition discards pre-reset buffered data on the same worker.
        unsafe {
            let _ = self.input.Unacquire();
        }
        self.needs_snapshot = true;
        self.retry_at = Instant::now();
    }

    fn disconnect(&mut self, id: usize, tx: &EventSender<Event>, epoch: u64) {
        if self.connected {
            send(tx, epoch, id, EventType::Disconnected);
        }
        self.connected = false;
        self.reset();
        self.retry_at = Instant::now() + RETRY_INTERVAL;
    }

    fn tick(&mut self, id: usize, tx: &EventSender<Event>, epoch: u64, force: bool) {
        if Instant::now() < self.retry_at {
            return;
        }
        if force {
            self.needs_snapshot = true;
        }
        if self.read(id, tx, epoch).is_err() {
            self.disconnect(id, tx, epoch);
        }
    }

    fn read(
        &mut self,
        id: usize,
        tx: &EventSender<Event>,
        epoch: u64,
    ) -> windows::core::Result<()> {
        // SAFETY: every method runs on the owner worker with live device/buffers.
        unsafe {
            if self.needs_snapshot {
                self.input.Acquire()?;
            }
            self.input.Poll()?;
            let mut events = [DIDEVICEOBJECTDATA::default(); BUFFER_EVENTS];
            let mut count = events.len() as u32;
            // Keep the raw success HRESULT: DI_BUFFEROVERFLOW == S_FALSE, which
            // windows-rs's Result wrapper would otherwise silently erase.
            let result = (self.input.vtable().GetDeviceData)(
                self.input.as_raw(),
                size_of::<DIDEVICEOBJECTDATA>() as u32,
                events.as_mut_ptr(),
                &mut count,
                0,
            );
            result.ok()?;
            if result == HRESULT(1) {
                send(tx, epoch, id, EventType::Overflow { dropped: 1 });
                self.reset();
                return Ok(());
            }
            if self.needs_snapshot {
                let mut state = [0u32; DATA_BYTES / 4];
                self.input
                    .GetDeviceState(DATA_BYTES as u32, state.as_mut_ptr().cast())?;
                // Drain-once precedes the snapshot. Later buffered events may
                // repeat state, but no future release is flushed after sampling.
                send(tx, epoch, id, EventType::Connected);
                self.connected = true;
                for control in &self.controls {
                    emit(control, value(&state, control), id, tx, epoch);
                }
                self.needs_snapshot = false;
            } else {
                for event in &events[..(count as usize).min(events.len())] {
                    if let Some(control) = self
                        .controls
                        .iter()
                        .find(|c| c.offset == event.dwOfs as usize)
                    {
                        emit(control, event.dwData, id, tx, epoch);
                    }
                }
            }
        }
        Ok(())
    }
}

fn header<T>(object: u32, how: u32) -> DIPROPHEADER {
    DIPROPHEADER {
        dwSize: size_of::<T>() as u32,
        dwHeaderSize: size_of::<DIPROPHEADER>() as u32,
        dwObj: object,
        dwHow: how,
    }
}

fn property(id: usize) -> *const GUID {
    id as *const GUID
}

fn data_format_objects() -> Vec<DIOBJECTDATAFORMAT> {
    let axes = [
        &GUID_XAxis,
        &GUID_YAxis,
        &GUID_ZAxis,
        &GUID_RxAxis,
        &GUID_RyAxis,
        &GUID_RzAxis,
        &GUID_Slider,
        &GUID_Slider,
    ];
    let optional_any = 0x80000000 | DIDFT_ANYINSTANCE;
    let mut objects = Vec::with_capacity(140);
    for (index, guid) in axes.into_iter().enumerate() {
        objects.push(DIOBJECTDATAFORMAT {
            pguid: guid,
            dwOfs: index as u32 * 4,
            dwType: optional_any | DIDFT_AXIS,
            dwFlags: DIDOI_ASPECTPOSITION,
        });
    }
    for index in 0..4 {
        objects.push(DIOBJECTDATAFORMAT {
            pguid: &GUID_POV,
            dwOfs: 32 + index * 4,
            dwType: optional_any | DIDFT_POV,
            dwFlags: 0,
        });
    }
    for index in 0..128 {
        objects.push(DIOBJECTDATAFORMAT {
            pguid: std::ptr::null(),
            dwOfs: 48 + index,
            dwType: optional_any | DIDFT_BUTTON,
            dwFlags: 0,
        });
    }
    objects
}

fn value(state: &[u32; DATA_BYTES / 4], control: &Control) -> u32 {
    let word = state[control.offset / 4];
    if matches!(control.kind, Kind::Button(_)) {
        (word >> (8 * (control.offset % 4))) & 0xff
    } else {
        word
    }
}

fn send(tx: &EventSender<Event>, epoch: u64, id: usize, event: EventType) {
    // The bounded queue exposes a sticky overflow marker to the consumer.
    let _ = tx.push_with_epoch(epoch, Event::new(id, event));
}

fn emit(control: &Control, value: u32, id: usize, tx: &EventSender<Event>, epoch: u64) {
    let axis = |code: EvCode, value: i32, reliable: bool| {
        let event = Event::new(id, EventType::AxisValueChanged(value, crate::EvCode(code)));
        if reliable {
            let _ = tx.push_with_epoch(epoch, event);
        } else {
            let _ = tx.push_latest_with_epoch(
                epoch,
                ((id as u64) << 32) | code.into_u32() as u64,
                event,
            );
        }
    };
    match control.kind {
        Kind::Axis(code) => axis(code, value as i32, false),
        Kind::Button(code) => send(
            tx,
            epoch,
            id,
            if value & 0x80 != 0 {
                EventType::ButtonPressed(crate::EvCode(code))
            } else {
                EventType::ButtonReleased(crate::EvCode(code))
            },
        ),
        Kind::Hat(x, y) => {
            let (vx, vy) = pov(value);
            // A hat is a button edge source: do not coalesce press/release pairs.
            axis(x, vx, true);
            axis(y, vy, true);
        }
    }
}
