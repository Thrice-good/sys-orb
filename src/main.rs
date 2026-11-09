// 发布版不弹控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use egui::viewport::{ViewportCommand, WindowLevel};
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

/// 悬浮球直径（逻辑像素）
const ORB: f32 = 132.0;

const CPU_BASE: Color32 = Color32::from_rgb(64, 208, 200);
const MEM_BASE: Color32 = Color32::from_rgb(167, 139, 250);
const TEXT_DIM: Color32 = Color32::from_rgb(148, 156, 172);

// ============================================================
// 系统采样：直接调 Win32，不引入 sysinfo
//
// 一次采样只有两个系统调用，合计耗时在微秒级。
// sysinfo 也能做同样的事，但它会带来几十个传递依赖，
// 而这个场景只需要「CPU 使用率」和「内存使用率」两个数字。
// ============================================================

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    fn ft_to_u64(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    /// 返回 (空闲累计时间, 总线累计时间)，单位是 100 纳秒
    pub fn cpu_times() -> Option<(u64, u64)> {
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut idle = zero;
        let mut kernel = zero;
        let mut user = zero;

        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }

        // 注意：kernel 时间里已经包含了 idle，
        // 所以「总时间」= kernel + user，而不是三者相加
        Some((ft_to_u64(idle), ft_to_u64(kernel) + ft_to_u64(user)))
    }

    /// 物理内存使用率（0~100）
    pub fn mem_load() -> Option<f32> {
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;

        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ok == 0 {
            return None;
        }
        Some(status.dwMemoryLoad as f32)
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn cpu_times() -> Option<(u64, u64)> {
        None
    }
    pub fn mem_load() -> Option<f32> {
        None
    }
}

/// CPU 用「两次采样的时间差」算，所以必须记住上一次的累计值。
struct Sampler {
    prev_idle: u64,
    prev_total: u64,
    cpu: f32,
    mem: f32,
}

impl Sampler {
    fn new() -> Self {
        // 先占一次基线，否则第一帧会把「自开机以来的平均值」当成当前占用
        let (prev_idle, prev_total) = sys::cpu_times().unwrap_or((0, 0));
        Self {
            prev_idle,
            prev_total,
            cpu: 0.0,
            mem: sys::mem_load().unwrap_or(0.0),
        }
    }

    fn sample(&mut self) {
        if let Some((idle, total)) = sys::cpu_times() {
            let d_idle = idle.saturating_sub(self.prev_idle);
            let d_total = total.saturating_sub(self.prev_total);
            self.prev_idle = idle;
            self.prev_total = total;
            if d_total > 0 {
                let busy = d_total.saturating_sub(d_idle);
                self.cpu = (busy as f32 / d_total as f32 * 100.0).clamp(0.0, 100.0);
            }
        }
        if let Some(m) = sys::mem_load() {
            self.mem = m;
        }
    }
}

// ============================================================
// 位置持久化：手写一行文本，不引入任何序列化依赖
// ============================================================

fn cfg_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(p) => PathBuf::from(p).join("sys-orb").join("pos.txt"),
        Err(_) => PathBuf::from("sys-orb-pos.txt"),
    }
}

fn load_pos() -> Option<Pos2> {
    let s = std::fs::read_to_string(cfg_path()).ok()?;
    let mut it = s.trim().split(',');
    let x: f32 = it.next()?.trim().parse().ok()?;
    let y: f32 = it.next()?.trim().parse().ok()?;
    // 分辨率变化后旧坐标可能落在屏幕外，超出合理范围就当没存过
    if x > -3000.0 && y > -3000.0 && x < 16000.0 && y < 16000.0 {
        Some(Pos2::new(x, y))
    } else {
        None
    }
}

fn save_pos(p: Pos2) {
    let path = cfg_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{},{}", p.x.round() as i32, p.y.round() as i32));
}

// ============================================================
// 去掉 Win11 的窗口描边
//
// 即使 with_decorations(false)，Windows 11 的 DWM 仍会给每个顶层窗口
// 画一条 1px 描边（窗口获得焦点时会变成主题色）。透明窗口上这条线
// 格外显眼，看起来就是一个方框。
//
// 三层处理，缺一不可：
//   1. DWMWA_BORDER_COLOR            = 34，设 DWMWA_COLOR_NONE，去掉 DWM 描边
//   2. DWMWA_WINDOW_CORNER_PREFERENCE = 33，设 DONOTROUND，去掉四角圆角裁剪
//   3. DWMWA_NCRENDERING_POLICY       =  2，设 DWMNCRP_DISABLED，
//      让 DWM 不要绘制非客户区 —— 顶部残留的那一根线就来自这里
//   4. 再直接清掉窗口样式里残留的边框位（WS_THICKFRAME / WS_CAPTION 等），
//      并用 SetWindowPos(SWP_FRAMECHANGED) 让改动立即重算。
//
// 客户区之外哪怕只有 1px 的非客户区，系统也会用边框色把它填上；透明窗口
// 圆外是空的，于是那一根线在深色桌面上就是一道白边。
//
// 以上都只在 Win11 生效，Win10 上调用返回失败，忽略即可。
// ============================================================

#[cfg(windows)]
fn strip_window_border(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, MARGINS};
    use windows_sys::Win32::Graphics::Gdi::CreateEllipticRgn;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, SetWindowRgn,
        GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
        WS_EX_WINDOWEDGE, WS_SYSMENU, WS_THICKFRAME,
    };

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(w) = handle.as_raw() else {
        return;
    };
    let hwnd = w.hwnd.get() as _;

    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_DONOTROUND: u32 = 1;
    const DWMWA_NCRENDERING_POLICY: u32 = 2;
    const DWMNCRP_DISABLED: u32 = 1;

    unsafe {
        let no_border: u32 = DWMWA_COLOR_NONE;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as _,
            &no_border as *const u32 as *const core::ffi::c_void,
            4,
        );

        let no_round: u32 = DWMWCP_DONOTROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as _,
            &no_round as *const u32 as *const core::ffi::c_void,
            4,
        );

        // 顶部那一根线的根源：DWM 仍在绘制非客户区
        let nc_off: u32 = DWMNCRP_DISABLED;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as _,
            &nc_off as *const u32 as *const core::ffi::c_void,
            4,
        );

        // 兜底：把窗口样式里残留的边框位直接清掉
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let style_mask =
            (WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_SYSMENU) as isize;
        if style & style_mask != 0 {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !style_mask);
        }

        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ex_mask = (WS_EX_CLIENTEDGE | WS_EX_WINDOWEDGE | WS_EX_DLGMODALFRAME) as isize;
        if ex & ex_mask != 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !ex_mask);
        }

        // 样式改了必须重算一次窗口框架，否则要等到下次尺寸变化才生效
        SetWindowPos(
            hwnd,
            core::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        // 撤掉 winit 为透明窗口设的"玻璃框架"扩展。
        // 那个扩展会让 DWM 在客户区顶部画一条 1px 的半透明框架线，
        // 而 OpenGL 画上去的内容是透明的，这条线就直接透出来 —— 顶部那根线的真身。
        let no_glass = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 0,
            cyBottomHeight: 0,
        };
        DwmExtendFrameIntoClientArea(hwnd, &no_glass);

        // 终极手段：把窗口形状裁成圆。
        // 圆外的像素从窗口层面就不存在，DWM 的描边、阴影、玻璃框架线
        // 无论怎么画都不会显示出来；顺带解决了四角拦截鼠标的问题。
        let mut rc: RECT = core::mem::zeroed();
        if GetClientRect(hwnd, &mut rc) != 0 {
            let cw = rc.right - rc.left;
            let ch = rc.bottom - rc.top;
            if cw > 8 && ch > 8 {
                let rgn = CreateEllipticRgn(0, 0, cw, ch);
                if !rgn.is_null() {
                    SetWindowRgn(hwnd, rgn, 1);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn strip_window_border(_frame: &eframe::Frame) {}

// ============================================================
// 颜色：低负载用主题色，越高越警示
// ============================================================

fn load_color(pct: f32, base: Color32) -> Color32 {
    if pct >= 85.0 {
        Color32::from_rgb(255, 92, 92)
    } else if pct >= 60.0 {
        Color32::from_rgb(245, 196, 81)
    } else {
        base
    }
}

/// egui 没有圆弧图元，用折线近似。段数按弧度长度取，短弧不会浪费顶点。
fn draw_ring(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    width: f32,
    frac: f32,
    color: Color32,
    track: Color32,
) {
    painter.circle_stroke(center, radius, Stroke::new(width, track));

    let frac = frac.clamp(0.0, 1.0);
    if frac <= 0.002 {
        return;
    }

    let n = ((frac * 96.0).ceil() as usize).max(2);
    let start = -std::f32::consts::FRAC_PI_2; // 从 12 点方向开始
    let span = std::f32::consts::TAU * frac;

    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let a = start + span * (i as f32 / n as f32);
        pts.push(center + Vec2::new(a.cos(), a.sin()) * radius);
    }
    painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
}

// ============================================================
// 应用
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Primary {
    Cpu,
    Mem,
}

struct Orb {
    sampler: Sampler,
    primary: Primary,

    /// 用户设定的基础间隔
    interval_base: f32,
    /// 当前实际间隔（数值稳定时会自动拉长）
    interval_cur: f32,
    stable_rounds: u32,
    samples_taken: u32,
    next_sample_at: f64,

    on_top: bool,
    start_pos: Option<Pos2>,
    pos_applied: bool,
    border_fixed: bool,
}

impl Orb {
    fn new(start_pos: Option<Pos2>) -> Self {
        Self {
            sampler: Sampler::new(),
            primary: Primary::Cpu,
            interval_base: 1.5,
            interval_cur: 1.5,
            stable_rounds: 0,
            samples_taken: 0,
            next_sample_at: 0.0,
            on_top: true,
            start_pos,
            pos_applied: false,
            border_fixed: false,
        }
    }

    /// 数值变化很小时把采样间隔指数拉长，静止时几乎不唤醒 CPU
    fn reschedule(&mut self, now: f64, prev: (f32, f32)) {
        self.samples_taken += 1;

        if self.samples_taken <= 3 {
            // 启动阶段先快速收敛，别一上来就降频
            self.stable_rounds = 0;
        } else {
            let dc = (self.sampler.cpu - prev.0).abs();
            let dm = (self.sampler.mem - prev.1).abs();
            if dc < 1.0 && dm < 0.5 {
                self.stable_rounds = (self.stable_rounds + 1).min(3);
            } else {
                self.stable_rounds = 0;
            }
        }

        let factor = 2f32.powi(self.stable_rounds as i32);
        self.interval_cur = (self.interval_base * factor).min(8.0);
        self.next_sample_at = now + self.interval_cur as f64;
    }
}

impl eframe::App for Orb {
    /// 透明清屏：圆外面一块像素都不画，露出桌面
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);

        // 窗口句柄在第一帧才一定可用，所以在这里补一次去描边
        if !self.border_fixed {
            self.border_fixed = true;
            strip_window_border(frame);
        }

        // 首次启动时把窗口挪到上次关闭的位置
        if !self.pos_applied {
            self.pos_applied = true;
            if let Some(p) = self.start_pos {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(p));
            }
        }

        // ---- 采样调度 ----
        if now >= self.next_sample_at {
            let prev = (self.sampler.cpu, self.sampler.mem);
            self.sampler.sample();
            self.reschedule(now, prev);
        }
        let wait = (self.next_sample_at - now).max(0.02) as f32;
        ctx.request_repaint_after(Duration::from_secs_f32(wait));

        // ---- 交互区域：整个球 ----
        let area = ui.max_rect();
        let d = area.width().min(area.height()).min(ORB);
        let center = area.center();
        let hit = Rect::from_center_size(center, Vec2::splat(d));
        let resp = ui.allocate_rect(hit, Sense::click_and_drag());

        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if resp.drag_stopped() {
            if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                save_pos(r.min);
            }
        }
        if resp.double_clicked() {
            self.primary = match self.primary {
                Primary::Cpu => Primary::Mem,
                Primary::Mem => Primary::Cpu,
            };
        }

        resp.context_menu(|ui| {
            ui.set_min_width(148.0);
            ui.label(egui::RichText::new("系统占用悬浮球").strong());
            ui.separator();

            ui.menu_button(
                format!("刷新间隔 · {:.1} 秒", self.interval_base),
                |ui| {
                    for v in [1.0_f32, 1.5, 2.0, 3.0, 5.0] {
                        let picked = ui
                            .selectable_label((self.interval_base - v).abs() < 0.01, format!("{v:.1} 秒"))
                            .clicked();
                        if picked {
                            self.interval_base = v;
                            self.interval_cur = v;
                            self.next_sample_at = 0.0;
                            self.samples_taken = 0;
                            ui.close();
                        }
                    }
                },
            );

            ui.label(format!("实际间隔 · {:.1} 秒", self.interval_cur));
            ui.separator();

            if ui
                .checkbox(&mut self.on_top, "总在最前")
                .changed()
            {
                let level = if self.on_top {
                    WindowLevel::AlwaysOnTop
                } else {
                    WindowLevel::Normal
                };
                ui.ctx().send_viewport_cmd(ViewportCommand::WindowLevel(level));
            }

            if ui.button("退出").clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        });

        // ---- 绘制 ----
        let painter = ui.painter();

        let shell_r = d * 0.5 - 3.0;
        painter.circle_filled(
            center,
            shell_r,
            Color32::from_rgba_unmultiplied(20, 23, 30, 232),
        );
        painter.circle_stroke(
            center,
            shell_r,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 26)),
        );

        let track = Color32::from_rgba_unmultiplied(255, 255, 255, 26);
        let outer_r = shell_r - 8.0;
        let inner_r = outer_r - 13.0;

        let (cpu_c, mem_c) = (
            load_color(self.sampler.cpu, CPU_BASE),
            load_color(self.sampler.mem, MEM_BASE),
        );

        draw_ring(painter, center, outer_r, 7.0, self.sampler.cpu / 100.0, cpu_c, track);
        draw_ring(painter, center, inner_r, 5.5, self.sampler.mem / 100.0, mem_c, track);

        let (big, big_color, small) = match self.primary {
            Primary::Cpu => (
                self.sampler.cpu,
                cpu_c,
                format!("MEM {:.0}%", self.sampler.mem),
            ),
            Primary::Mem => (
                self.sampler.mem,
                mem_c,
                format!("CPU {:.0}%", self.sampler.cpu),
            ),
        };

        painter.text(
            center + Vec2::new(0.0, -7.0),
            Align2::CENTER_CENTER,
            format!("{big:.0}%"),
            FontId::proportional(30.0),
            big_color,
        );
        painter.text(
            center + Vec2::new(0.0, 18.0),
            Align2::CENTER_CENTER,
            small,
            FontId::proportional(10.5),
            TEXT_DIM,
        );
    }
}

fn main() -> eframe::Result<()> {
    let start_pos = load_pos();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([ORB, ORB])
            .with_min_inner_size([ORB, ORB])
            .with_max_inner_size([ORB, ORB])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_taskbar(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_title("系统占用"),
        ..Default::default()
    };

    eframe::run_native(
        "系统占用",
        options,
        Box::new(move |_cc| Ok(Box::new(Orb::new(start_pos)))),
    )
}
