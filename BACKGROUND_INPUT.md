# Windows background controller input

The Windows `xinput` backend now combines two background-safe paths:

- XInput polls controllers 0-3 regardless of window focus (unchanged).
- A nonexclusive `DISCL_BACKGROUND | DISCL_NONEXCLUSIVE` DirectInput path covers
  devices without an XInput interface. It owns a hidden STATIC window on the
  existing polling worker, sets the SDL joystick data format, reads VID/PID and
  per-axis ranges, skips interfaces whose device path contains `IG_` (XInput,
  per SDL's deduplication), and emits buttons, axes and hats through the same
  bounded queue with SDL-compatible GUIDs so the built-in SDL mappings apply.

All COM objects are created, polled and destroyed on the worker thread. Devices
are re-enumerated about once per second; acquisition loss, unplug and reset
discard buffered data and force an authoritative snapshot, so held buttons and
axes are reseeded instead of sticking. The backend now reports
`IS_Y_AXIS_REVERSED = true`, matching SDL, WGI, IOHID and evdev.

Verified so far: Windows-target clippy for `xinput` and default `wgi` features,
rustfmt, and the platform-independent hat/GUID/dedup/Y-reflection unit tests.

Still required before the change can be called verified on hardware: physical
XInput and DirectInput controllers, foreground/background switching, idle CPU,
reset/shutdown latency and long-run stability.
