//! The Wintab packet reader: the pen path when the driver's "Use Windows Ink" is off.
//!
//! Off, the Wacom driver stops sending pen frames through `WM_POINTER` (winit then sees only the
//! plain mouse it synthesizes: moves, the tip as the left button, the barrel as the right) and
//! serves the pen's data through the Wintab API instead (`wintab32.dll`, installed by the
//! driver), pressure already run through its Tip Feel curve and click thresholds at the sensor's
//! full resolution. This module therefore synthesizes no mouse messages and applies no feel
//! curve: it opens a context on the window (`CXO_MESSAGES`, packets: cursor, buttons, x, y,
//! normal pressure), subclasses the window for `WT_PACKET` and `WT_PROXIMITY` and reports each
//! reading through the same callback the other platforms use — pen in range (hovering or
//! touching) is `Some`, back to mouse is `None`. The DLL is loaded dynamically, so a machine
//! without a Wintab driver fails [`Monitor::install`] cleanly.
//!
//! Tilt is not read yet (packets would need `PK_ORIENTATION`): the sample carries none, like a
//! mouse.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::core::w;

use crate::{Error, Sample, deliver};

/// `WT_DEFBASE` (wintab.h): the default message base; `WT_PACKET` and `WT_PROXIMITY` are its
/// offsets.
const WT_DEFBASE: u32 = 0x7FF0;
const WT_PACKET: u32 = WT_DEFBASE;
const WT_PROXIMITY: u32 = WT_DEFBASE + 5;

/// The subclass id (`'pcwt'`); identifies this subclass for removal.
const SUBCLASS_ID: usize = 0x7063_7774;

/// wintab.h constants: the default context to copy, the cursor category and its type entry, the
/// device's pressure axis, and the packet fields this reader asks for (in the wintab bit order
/// the packet lays them out in).
const WTI_DEFCONTEXT: u32 = 3;
const WTI_DEVICES: u32 = 100;
const WTI_CURSORS: u32 = 200;
const DVC_NPRESSURE: u32 = 15;
const CSR_TYPE: u32 = 20;
const PK_CURSOR: u32 = 0x0020;
const PK_BUTTONS: u32 = 0x0040;
const PK_X: u32 = 0x0080;
const PK_Y: u32 = 0x0100;
const PK_NORMAL_PRESSURE: u32 = 0x0400;
const PKT_FIELDS: u32 = PK_CURSOR | PK_BUTTONS | PK_X | PK_Y | PK_NORMAL_PRESSURE;
/// `CXO_MESSAGES`: this context wants `WT_*` messages. Deliberately *without* `CXO_SYSTEM`: the
/// driver's own system context keeps moving the cursor and clicking, which is what the unchecked
/// Ink box promises.
const CXO_MESSAGES: u32 = 0x0004;

/// A Wintab context handle.
#[allow(clippy::upper_case_acronyms)]
type HCTX = usize;

/// The Wintab functions this reader needs (the spec's exports on `wintab32.dll`).
type WTInfoFn = unsafe extern "system" fn(u32, u32, *mut c_void) -> u32;
type WTOpenFn = unsafe extern "system" fn(HWND, *mut LOGCONTEXT, i32) -> HCTX;
type WTCloseFn = unsafe extern "system" fn(HCTX) -> i32;
type WTPacketFn = unsafe extern "system" fn(HCTX, u32, *mut c_void) -> i32;

/// `LOGCONTEXTW` (wintab.h): the context description read as the default and modified.
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
#[derive(Clone, Copy)]
struct LOGCONTEXT {
    name: [u16; 40], // LCNAMELEN
    options: u32,
    status: u32,
    locks: u32,
    msg_base: u32,
    device: u32,
    pkt_rate: u32,
    pkt_data: u32,
    pkt_mode: u32,
    move_mask: u32,
    btn_dn_mask: u32,
    btn_up_mask: u32,
    in_org_x: i32,
    in_org_y: i32,
    in_org_z: i32,
    in_ext_x: i32,
    in_ext_y: i32,
    in_ext_z: i32,
    out_org_x: i32,
    out_org_y: i32,
    out_org_z: i32,
    out_ext_x: i32,
    out_ext_y: i32,
    out_ext_z: i32,
    sens_x: i32,
    sens_y: i32,
    sens_z: i32,
    sys_mode: i32,
    sys_org_x: i32,
    sys_org_y: i32,
    sys_ext_x: i32,
    sys_ext_y: i32,
    sys_sens_x: i32,
    sys_sens_y: i32,
}

/// `AXIS` (wintab.h): a device axis range (the pressure scale).
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(clippy::upper_case_acronyms)]
struct AXIS {
    ax_min: i32,
    ax_max: i32,
    ax_units: u32,
    ax_resolution: u32,
}

/// One packet, the fields of `PKT_FIELDS` in bit order.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Packet {
    cursor: u32,
    buttons: u32,
    x: u32,
    y: u32,
    pressure: u32,
}

/// What the proc reads: the user callback, the Wintab calls and context, the pressure scale, and
/// the pen state it tracks between messages (the cursor type cache and whether the pen is in
/// range). Only the window's thread touches it.
struct Data {
    callback: Box<dyn Fn(Option<Sample>)>,
    wt_info: WTInfoFn,
    wtpacket: WTPacketFn,
    wt_close: WTCloseFn,
    hctx: HCTX,
    pressure_max: f32,
    /// The last packet's cursor index and its `CSR_TYPE` (queried once per cursor; `WT_PROXIMITY`
    /// names no cursor, so packets are where the types are learned).
    last_cursor: Cell<(u32, u32)>,
    /// The pen is in hover range or in contact (a `WT_PROXIMITY` enter, until its leave).
    in_range: Cell<bool>,
}

/// Keeps the Wintab context and the subclass alive; dropping it closes the context and removes
/// the subclass (keep it until the window is gone). The proc runs on the window's thread, so
/// install on that thread.
pub struct Monitor {
    hwnd: HWND,
    /// The `Box<Data>` the proc reads; freed in `Drop` after the subclass is removed.
    data: *mut Data,
}

impl Monitor {
    /// Open a Wintab context on the window (the eframe/winit window's `HWND`) and report pen
    /// samples through `callback`. Call on the window's thread. Fails cleanly on a machine
    /// without a Wintab driver (`wintab32.dll` or its exports missing, no devices).
    pub fn install(hwnd: *mut c_void, callback: impl Fn(Option<Sample>) + 'static) -> Result<Self, Error> {
        let hwnd = HWND(hwnd);
        if hwnd.is_invalid() {
            return Err(Error::Unsupported("no window handle".into()));
        }
        // SAFETY: loads the driver-installed Wintab shim by name; a machine without one errors.
        let wintab = unsafe { LoadLibraryW(w!("wintab32.dll")) }.map_err(|_| Error::Unsupported("wintab32.dll is not installed (no Wintab driver)".into()))?;
        // SAFETY: `wintab` stays loaded for the process's life; a shim without the spec's exports
        // transmutes to a call that would fault, but every Wintab driver ships them (they are the
        // API), and `GetProcAddress` failing is handled. `FARPROC` is non-null on success.
        let far = |name: &str| {
            let nul: Vec<u8> = name.bytes().chain([0]).collect();
            // SAFETY: `nul` is a live null-terminated string for the duration of the call.
            unsafe { GetProcAddress(wintab, windows::core::PCSTR(nul.as_ptr())) }.ok_or_else(|| Error::Platform(format!("{name} not found in wintab32.dll")))
        };
        let wt_info: WTInfoFn = unsafe { std::mem::transmute(far("WTInfoW")?) };
        // SAFETY: a plain API call; `0, 0, null` is the documented presence check.
        if unsafe { wt_info(0, 0, std::ptr::null_mut()) } == 0 {
            return Err(Error::Unsupported("no Wintab devices (WTInfo reports none)".into()));
        }

        // The default context, tuned for this window: our packet fields as messages, without the
        // system-cursor option (the driver's own system context keeps doing the mouse).
        // SAFETY: an all-zero bit pattern is valid for this plain-data struct; `WTInfo` overwrites
        // it with the default context below.
        let mut lc: LOGCONTEXT = unsafe { std::mem::zeroed() };
        // SAFETY: `lc` outlives the call and the API writes at most `sizeof(LOGCONTEXT)`.
        if unsafe { wt_info(WTI_DEFCONTEXT, 0, &mut lc as *mut LOGCONTEXT as *mut c_void) } == 0 {
            return Err(Error::Platform("WTInfo WTI_DEFCONTEXT failed".into()));
        }
        for (i, ch) in "Photocraft".encode_utf16().take(39).enumerate() {
            lc.name[i] = ch;
        }
        lc.options = CXO_MESSAGES;
        lc.msg_base = WT_DEFBASE;
        lc.pkt_data = PKT_FIELDS;
        lc.pkt_mode = 0;
        lc.move_mask = PK_X | PK_Y;
        // The pressure scale (the driver's Tip-Feel'ed output range; 0..ax_max).
        let mut axis = AXIS::default();
        // SAFETY: `axis` outlives the call and the API writes at most `sizeof(AXIS)`.
        let axis_ok = unsafe { wt_info(WTI_DEVICES, DVC_NPRESSURE, &mut axis as *mut AXIS as *mut c_void) } != 0;
        let pressure_max = if axis_ok && axis.ax_max > 0 { axis.ax_max as f32 } else { 1023.0 };

        // SAFETY: the name/fields above satisfy the spec; the context is closed in `Drop`.
        let wt_open: WTOpenFn = unsafe { std::mem::transmute(far("WTOpenW")?) };
        // SAFETY: `hwnd` is the live window the caller keeps alive; `lc` lives for the call.
        let hctx = unsafe { wt_open(hwnd, &mut lc, 1) };
        if hctx == 0 {
            return Err(Error::Platform("WTOpen failed".into()));
        }
        let wt_packet: WTPacketFn = unsafe { std::mem::transmute(far("WTPacket")?) };
        let wt_close: WTCloseFn = unsafe { std::mem::transmute(far("WTClose")?) };
        let data = Box::into_raw(Box::new(Data {
            callback: Box::new(callback),
            wt_info,
            wtpacket: wt_packet,
            wt_close,
            hctx,
            pressure_max,
            last_cursor: Cell::new((u32::MAX, 0)),
            in_range: Cell::new(false),
        }));
        // SAFETY: `hwnd` is live; `data` is a `Box<Data>` the proc reads until `Drop` removes the
        // subclass; `subclass_proc` is a plain `extern "system"` fn; `SUBCLASS_ID` identifies it.
        if !unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, data as usize) }.as_bool() {
            // SAFETY: the subclass was not installed, so the proc will never read `data`; the
            // freshly opened context must not leak. `WTClose` comes from the loaded shim.
            unsafe { wt_close(hctx) };
            drop(unsafe { Box::from_raw(data) });
            return Err(Error::Platform("SetWindowSubclass failed".into()));
        }
        Ok(Monitor { hwnd, data })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: the subclass with `SUBCLASS_ID` was installed on this window's thread, and
        // `Drop` runs there too. Removing it ends the callbacks, so the proc cannot read `data`
        // while (and after) it is freed. If the window is already destroyed this fails and the
        // data box and the context leak, which is harmless (nothing reads them).
        unsafe {
            let _ = RemoveWindowSubclass(self.hwnd, Some(subclass_proc), SUBCLASS_ID);
            let data = Box::from_raw(self.data);
            (data.wt_close)(data.hctx);
        }
    }
}

/// Is this packet's cursor the pen's inverted end? Some drivers mark the index itself (the
/// classic `0x80` inverted-id bit); spec-wise, the index selects a `WTI_CURSORS` entry whose
/// `CSR_TYPE` says the tool: `0x0F06` masks the Wacom tool family (`0x802` = stylus) and bit
/// `0x08` is the inverted orientation (a grip pen reports `0x822`, its eraser end `0x82a`).
fn eraser_end(data: &Data, cursor: u32) -> bool {
    if cursor & 0x80 != 0 {
        return true;
    }
    let (last, mut csr_type) = data.last_cursor.get();
    if last != cursor {
        csr_type = 0;
        let mut read = 0u32;
        // SAFETY: a plain API call; `read` outlives it and the API writes at most a `u32`.
        if unsafe { (data.wt_info)(WTI_CURSORS + cursor, CSR_TYPE, &mut read as *mut u32 as *mut c_void) } != 0 {
            csr_type = read;
        }
        data.last_cursor.set((cursor, csr_type));
    }
    csr_type & 0x0006 == 0x0002 && csr_type & 0x0008 != 0
}

/// The subclass proc: reports Wintab pen packets through the callback and passes every other
/// message on to winit (the driver's mouse emulation needs nothing from here).
unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, dwref: usize) -> LRESULT {
    if dwref != 0 && (msg == WT_PACKET || msg == WT_PROXIMITY) {
        // SAFETY: `dwref` is the `Box<Data>` `install` leaked; it stays alive until `Drop` removes
        // this subclass (or the window is destroyed, which ends the callbacks first).
        let data = unsafe { &*(dwref as *const Data) };
        if msg == WT_PACKET {
            let mut pkt = Packet::default();
            // SAFETY: `data.hctx` is the open context, `wparam` the packet message's serial, and
            // `pkt` lives for the call with exactly the fields `lc.pkt_data` asked for. A failed
            // read is ignored: one unread packet must not lift the pen mid-stroke.
            if unsafe { (data.wtpacket)(data.hctx, (wparam.0 & 0xFFFF) as u32, &mut pkt as *mut Packet as *mut c_void) } != 0 {
                // On the Wintab path the pressure *is* the contact: the driver reports 0 out of
                // range (it has already applied its click thresholds), and the mouse it
                // synthesizes carries the buttons (the barrel arrives as a real right click).
                let eraser = eraser_end(data, pkt.cursor);
                if pkt.pressure > 0 {
                    data.in_range.set(true);
                    deliver(
                        &*data.callback,
                        Some(Sample { pressure: (pkt.pressure as f32 / data.pressure_max).clamp(0.0, 1.0), tilt_x: 0.0, tilt_y: 0.0, rotation: 0.0, eraser }),
                    );
                } else if data.in_range.get() {
                    // Hovering: a pen is near, at no pressure.
                    deliver(&*data.callback, Some(Sample { pressure: 0.0, eraser, ..Sample::default() }));
                }
            }
            return LRESULT(0);
        }
        // WT_PROXIMITY: wparam's low word is nonzero entering the hover range, 0 leaving. Entering
        // reports the hovering pen (its end is learned with the first packet); leaving ends any
        // contact (the same "pen left" the Windows Ink path reports as a lift).
        let entering = wparam.0 & 0xFFFF != 0;
        data.in_range.set(entering);
        deliver(&*data.callback, entering.then_some(Sample { pressure: 0.0, ..Sample::default() }));
        return LRESULT(0);
    }
    // SAFETY: plain pass-through; winit's wndproc must see every message that isn't ours.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
