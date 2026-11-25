//! 显示器枚举（直接用 user32，避免为 M0 引入额外依赖）。

use std::os::raw::c_void;

use screenwall_proto::DisplayInfo;

const MONITORINFOF_PRIMARY: u32 = 1;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

/// 一块屏幕在虚拟桌面坐标系里的矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl MonitorRect {
    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfoExW {
    cb_size: u32,
    rc_monitor: Rect,
    rc_work: Rect,
    dw_flags: u32,
    sz_device: [u16; 32],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplayMonitors(
        hdc: *mut c_void,
        clip: *const Rect,
        callback: Option<unsafe extern "system" fn(isize, *mut c_void, *mut Rect, isize) -> i32>,
        data: isize,
    ) -> i32;

    fn GetMonitorInfoW(hmonitor: isize, info: *mut MonitorInfoExW) -> i32;

    fn GetSystemMetrics(index: i32) -> i32;
}

unsafe extern "system" fn collect_monitor(
    hmonitor: isize,
    _hdc: *mut c_void,
    _clip: *mut Rect,
    data: isize,
) -> i32 {
    let out = &mut *(data as *mut Vec<DisplayInfo>);
    let mut info: MonitorInfoExW = std::mem::zeroed();
    info.cb_size = std::mem::size_of::<MonitorInfoExW>() as u32;
    if GetMonitorInfoW(hmonitor, &mut info) != 0 {
        out.push(DisplayInfo {
            index: 0,
            w: (info.rc_monitor.right - info.rc_monitor.left).max(0) as u32,
            h: (info.rc_monitor.bottom - info.rc_monitor.top).max(0) as u32,
            primary: info.dw_flags & MONITORINFOF_PRIMARY != 0,
            x: info.rc_monitor.left,
            y: info.rc_monitor.top,
        });
    }
    1
}

/// 枚举所有显示器，主显示器排在 index 0。
pub fn list_displays() -> Vec<DisplayInfo> {
    let mut displays: Vec<DisplayInfo> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_monitor),
            &mut displays as *mut _ as isize,
        )
    };

    if ok == 0 || displays.is_empty() {
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        displays.push(DisplayInfo {
            index: 0,
            w: w.max(0) as u32,
            h: h.max(0) as u32,
            primary: true,
            x: 0,
            y: 0,
        });
    }

    displays.sort_by_key(|display| !display.primary);
    for (index, display) in displays.iter_mut().enumerate() {
        display.index = index as u32;
    }
    displays
}

/// 取指定显示器的矩形（主屏为 index 0）。索引越界时返回 `None`。
pub fn monitor_rect(index: u32) -> Option<MonitorRect> {
    list_displays()
        .into_iter()
        .find(|display| display.index == index)
        .map(|display| MonitorRect {
            x: display.x,
            y: display.y,
            w: display.w,
            h: display.h,
        })
        .filter(|rect| !rect.is_empty())
}

/// 整块虚拟桌面的矩形；多显示器时原点可能为负。
pub fn virtual_rect() -> Option<MonitorRect> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w > 0 && h > 0 {
            return Some(MonitorRect {
                x,
                y,
                w: w as u32,
                h: h as u32,
            });
        }
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w > 0 && h > 0 {
            Some(MonitorRect {
                x: 0,
                y: 0,
                w: w as u32,
                h: h as u32,
            })
        } else {
            None
        }
    }
}
