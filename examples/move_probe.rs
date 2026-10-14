//! A 卡（`abb-v11-bubble-follow-lag`）的**本机实测台**：验证「拖窗期间 winit 到底交不交
//! `Moved`」以及每次跟随的 FFI 开销。
//!
//! 为什么需要它：卡片要求「先量再改，不许凭感觉」；而真机手动拖窗口既不可自动复现、也不能在
//! 这个环境里让人去拖。本台子用**同一条系统机制**自动复现拖动：
//! `-[NSWindow performWindowDragWithEvent:]` 与「用户拖标题栏」都跑在
//! `NSEventTrackingRunLoopMode` 的嵌套 run loop 里，而 winit 是否把事件交到应用层只取决于
//! 「当时 run loop 是什么模式 + 事件处理器是否正在执行」——两个条件本台子都能精确复现：
//! 我们在**事件处理器内部**（`user_event`）跑一段
//! `CFRunLoopRunInMode(NSEventTrackingRunLoopMode, …)`，并用挂在 **common modes** 上的
//! `CFRunLoopTimer` 每 8ms 移动一次窗口（= 模拟 WindowServer 拖动窗口）。
//!
//! 它只开两个小窗（~1s 自动退出），不写任何文件、不碰 `~/.agent-bridge`，
//! 与 `run_gui`（会改用户 LaunchAgents）完全不同。
//!
//! 用法：`cargo run --example move_probe`

#[cfg(not(target_os = "macos"))]
fn main() {
    // winit 的 macOS 事件循环行为是唯一被测对象；其它平台没有这个机制。
    println!("move_probe 只在 macOS 上有意义（NSEventTrackingRunLoopMode / winit macOS 事件循环）");
}

#[cfg(target_os = "macos")]
fn main() {
    real::run();
}

#[cfg(target_os = "macos")]
mod real {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use winit::application::ApplicationHandler;
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::event::{StartCause, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
    use winit::window::{Window, WindowId};

    /// 模拟拖动用的帧数/节拍（= 60fps 量级的一半，便于把「队列化」现象放大）。
    const MOVES: usize = 30;
    const TICK: Duration = Duration::from_millis(8);
    /// 嵌套 run loop 的时长（覆盖 MOVES×TICK + 余量）。
    const TRACK_SECS: f64 = 0.45;
    /// 每次调用的计时轮数。
    const ITERS: usize = 20_000;
    /// 嵌套循环结束后，给 default mode 多少时间把排队事件跑完。
    const FLUSH_MS: u64 = 300;

    #[derive(Debug)]
    enum User {
        Go,
    }

    /// 一次跑完后的耗时实测值（事件计数在阶段一就印了）。
    struct Measured {
        outer_ns: f64,
        inner_ns: f64,
        monitor_ns: f64,
        visible_ns: f64,
        setpos_ns: f64,
        borrow_ns: f64,
    }

    struct App {
        proxy: EventLoopProxy<User>,
        dialog: Option<Arc<Window>>,
        bubble: Option<Arc<Window>>,
        /// 应用层在**嵌套 run loop 内**收到的 Moved 数。
        moved_during: Rc<Cell<usize>>,
        /// 应用层在嵌套循环**结束后**补齐收到的 Moved 数。
        moved_after: Rc<Cell<usize>>,
        /// 嵌套循环内 common-modes 定时器的触发次数。
        ticks_during: Rc<Cell<usize>>,
        /// 全部 Moved（含创建期）：用来确认「事件到底有没有被交付」。
        moved_total: Rc<Cell<usize>>,
        /// 事件回调据此分流计数（嵌套循环期间的算 during，之后算 after）。
        tracked: Rc<Cell<bool>>,
        stage: u8,
        go_done: bool,
        track_ms: f64,
        /// 模拟拖动前后对话窗的实际位置（验证「common-modes 回调里 set 位置真的生效」）。
        pos_before: (i32, i32),
        pos_after: (i32, i32),
        /// 定时器最后一次要求写入的 x（期望 = 起始 x + MOVES）。
        last_asked_x: Rc<Cell<i32>>,
        measured: Option<Measured>,
        report_at: Option<Instant>,
    }

    impl App {
        fn measure(&mut self) {
            let dialog = self.dialog.clone().expect("dialog");
            let n = |d: Duration| d.as_secs_f64() * 1e9 / ITERS as f64;
            let bench = |f: &mut dyn FnMut()| {
                let t = Instant::now();
                for _ in 0..ITERS {
                    f();
                }
                n(t.elapsed())
            };
            let mut op = || {
                let _ = dialog.outer_position();
            };
            let outer_ns = bench(&mut op);
            let mut isz = || {
                let _ = dialog.inner_size();
            };
            let inner_ns = bench(&mut isz);
            let mut cm = || {
                let _ = dialog.current_monitor();
            };
            let monitor_ns = bench(&mut cm);
            let mut vis = || {
                let _ = dialog.is_visible();
            };
            let visible_ns = bench(&mut vis);
            let x = Cell::new(0i32);
            let mut sp = || {
                x.set(x.get() + 1);
                dialog.set_outer_position(PhysicalPosition::new(x.get(), 40));
            };
            let setpos_ns = bench(&mut sp);
            // Slint 的 `with_winit_window` 内部就是「借出 + Arc::clone + 放回」+ downcast，
            // 这里用同款操作量出它的量级（downcast 是一次 Any 比较，可忽略）。
            let cell: RefCell<Option<Arc<Window>>> = RefCell::new(Some(dialog));
            let mut borrow_op = || {
                let g = cell.borrow();
                let w = g.as_ref().expect("some").clone();
                drop(g);
                drop(w);
            };
            let borrow_ns = bench(&mut borrow_op);
            let m = Measured {
                outer_ns,
                inner_ns,
                monitor_ns,
                visible_ns,
                setpos_ns,
                borrow_ns,
            };
            self.measured = Some(m);
        }

        /// 阶段一：事件交付量（在**跑基准之前**印，避免基准自己造出的 Moved 污染计数）。
        fn report_counts(&self, track_ms: f64, ticks_during: usize, moved_during: usize) {
            println!("== A 卡实测（macOS / winit 0.30.13 / 本机） ==");
            println!(
                "模拟拖动：common-modes 定时器每 {TICK:?} 移 1px × {MOVES}，\
                 嵌套 run loop = NSEventTrackingRunLoopMode（{track_ms:.0}ms）"
            );
            println!(
                "  · 嵌套循环内 common-modes 定时器触发：{ticks_during}（⇒ 该挂点在 event-tracking mode 下**可用**）"
            );
            println!(
                "  · 嵌套循环内应用收到的 Moved：{moved_during}（⇒ 拖动期间应用层拿不到几何事件）"
            );
            println!(
                "  · 返回 default mode（+{FLUSH_MS}ms 后）补齐收到的 Moved：{}（延迟交付，不是连续跟随）",
                self.moved_after.get()
            );
            println!(
                "  · 全部 Moved（含创建期）合计：{}（用于确认事件确实被交付过）",
                self.moved_total.get()
            );
            println!(
                "  · 模拟拖动是否真的移动了窗口：x {:?} → {:?}，定时器最后要求写入 x={} \
                 （⇒ common-modes 回调里 `set_outer_position` 在 event-tracking mode 下**生效**）",
                self.pos_before,
                self.pos_after,
                self.last_asked_x.get()
            );
        }

        fn report(&self, m: &Measured) {
            let old_ns = 3.0 * m.borrow_ns
                + 2.0 * m.outer_ns
                + 2.0 * m.inner_ns
                + m.monitor_ns
                + m.setpos_ns;
            let new_still_ns = m.visible_ns + m.outer_ns + m.inner_ns;
            let new_move_ns = new_still_ns + m.outer_ns + m.inner_ns + m.setpos_ns;
            println!("每调用耗时（{ITERS} 次平均）：");
            println!("  outer_position()      : {:.0} ns", m.outer_ns);
            println!("  inner_size()          : {:.0} ns", m.inner_ns);
            println!("  current_monitor()     : {:.0} ns", m.monitor_ns);
            println!("  is_visible()          : {:.0} ns", m.visible_ns);
            println!("  set_outer_position()  : {:.0} ns", m.setpos_ns);
            println!(
                "  借出+Arc克隆（Slint with_winit_window 同款操作）: {:.0} ns",
                m.borrow_ns
            );
            println!("改前每次跟随（事件路径，2 窗口矩形 + 显示器查询 + 写位）≈ {old_ns:.0} ns");
            println!("改后每拍（common-modes 定时器）：");
            println!("  · 对话窗矩形未变（多数拍）≈ {new_still_ns:.0} ns");
            println!(
                "  · 矩形变化（读气泡 + 写位；工作区命中缓存 ⇒ 0 次显示器查询）≈ {new_move_ns:.0} ns"
            );
            println!(
                "  · 节拍 16ms ⇒ 静止时 ≈ {:.3}% CPU（单核口径，仅计这些 FFI）",
                new_still_ns / 16_000_000.0 * 100.0
            );
        }
    }

    impl ApplicationHandler<User> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.dialog.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_visible(true)
                .with_inner_size(PhysicalSize::new(64, 48))
                .with_position(PhysicalPosition::new(40, 40));
            self.dialog = Some(Arc::new(event_loop.create_window(attrs.clone()).expect("dialog")));
            self.bubble = Some(Arc::new(
                event_loop
                    .create_window(attrs.with_position(PhysicalPosition::new(400, 40)))
                    .expect("bubble"),
            ));
            self.proxy.send_event(User::Go).expect("proxy");
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::Moved(_) => {
                    self.moved_total.set(self.moved_total.get() + 1);
                    if self.tracked.get() {
                        self.moved_after.set(self.moved_after.get() + 1);
                    } else {
                        self.moved_during.set(self.moved_during.get() + 1);
                    }
                }
                WindowEvent::CloseRequested => event_loop.exit(),
                _ => {}
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: User) {
            let User::Go = event;
            if self.go_done {
                return;
            }
            self.go_done = true;
            let dialog = self.dialog.clone().expect("dialog");
            // 计数器清零：窗口创建/首次 show 自己也会产生 Moved，不能算进拖动模拟。
            self.moved_during.set(0);
            self.ticks_during.set(0);
            // —— ① 嵌套 event-tracking run loop + common-modes 定时器 ——
            // 注意：我们**正处在 winit 的事件处理器内部**——这正是 `drag_window()` 被调用时
            // 的状态（卡内根因第 2/3 点）。
            let mover = dialog;
            let ticks = self.ticks_during.clone();
            let asked = self.last_asked_x.clone();
            let timer = common_mode_timer(TICK, move || {
                let n = ticks.get();
                if n >= MOVES {
                    return;
                }
                ticks.set(n + 1);
                // 每拍把 x 自增 1（不读回窗口位置，避免「读回陈旧值 ⇒ 原地写」的歧义）。
                let next = asked.get() + 1;
                asked.set(next);
                mover.set_outer_position(PhysicalPosition::new(next, 40));
            });
            self.pos_before = self
                .dialog
                .as_ref()
                .and_then(|d| d.outer_position().ok())
                .map(|p| (p.x, p.y))
                .unwrap_or((0, 0));
            self.last_asked_x.set(self.pos_before.0);
            let t0 = Instant::now();
            nested_event_tracking_loop(TRACK_SECS);
            self.pos_after = self
                .dialog
                .as_ref()
                .and_then(|d| d.outer_position().ok())
                .map(|p| (p.x, p.y))
                .unwrap_or((0, 0));
            self.track_ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.tracked.set(true);
            drop(timer);
            // 让 default mode 把排队事件跑完，然后再量/报告（不能睡主线程，那会把 run loop 堵住）。
            self.report_at = Some(Instant::now() + Duration::from_millis(FLUSH_MS));
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(FLUSH_MS),
            ));
        }

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            if let StartCause::ResumeTimeReached { .. } = cause {
                if self.report_at.is_some_and(|t| Instant::now() >= t) {
                    match self.stage {
                        0 => {
                            self.report_counts(
                                self.track_ms,
                                self.ticks_during.get(),
                                self.moved_during.get(),
                            );
                            self.stage = 1;
                            self.report_at = Some(Instant::now() + Duration::from_millis(FLUSH_MS));
                            event_loop.set_control_flow(ControlFlow::WaitUntil(
                                Instant::now() + Duration::from_millis(FLUSH_MS),
                            ));
                        }
                        _ => {
                            if self.measured.is_none() {
                                self.measure();
                            }
                            if let Some(m) = self.measured.as_ref() {
                                self.report(m);
                            }
                            self.report_at = None;
                            event_loop.exit();
                        }
                    }
                }
            }
        }
    }

    pub fn run() {
        let event_loop = EventLoop::<User>::with_user_event()
            .build()
            .expect("event loop");
        let proxy = event_loop.create_proxy();
        let mut app = App {
            proxy,
            dialog: None,
            bubble: None,
            moved_during: Rc::new(Cell::new(0)),
            moved_after: Rc::new(Cell::new(0)),
            moved_total: Rc::new(Cell::new(0)),
            ticks_during: Rc::new(Cell::new(0)),
            tracked: Rc::new(Cell::new(false)),
            stage: 0,
            go_done: false,
            track_ms: 0.0,
            pos_before: (0, 0),
            pos_after: (0, 0),
            last_asked_x: Rc::new(Cell::new(0)),
            measured: None,
            report_at: None,
        };
        event_loop.run_app(&mut app).expect("run_app");
    }

    /// 在**当前（事件处理器内部）**再跑一段 `NSEventTrackingRunLoopMode` 的嵌套 run loop。
    /// 与 `-[NSWindow performWindowDragWithEvent:]` 的 run loop 模式一致。
    /// （该模式的常量由 Foundation/AppKit 导出；CoreFoundation 只导出
    /// `kCFRunLoopDefaultMode` / `kCFRunLoopCommonModes`。）
    fn nested_event_tracking_loop(secs: f64) {
        use std::ffi::c_void;
        unsafe extern "C" {
            fn CFRunLoopRunInMode(
                mode: *const c_void,
                seconds: f64,
                return_after_source: bool,
            ) -> i32;
            static NSEventTrackingRunLoopMode: *const c_void;
        }
        unsafe {
            CFRunLoopRunInMode(NSEventTrackingRunLoopMode, secs, false);
        }
    }

    /// 与 `src/platform.rs::common_mode_timer` **同一份实现**（example 引用不到 bin 的模块，
    /// 故此处逐字复制；生产那份是承重件）。
    fn common_mode_timer(interval: Duration, cb: impl FnMut() + 'static) -> CommonModeTimer {
        use std::ffi::c_void;

        type CfIndex = isize;
        #[repr(C)]
        struct TimerContext {
            version: CfIndex,
            info: *mut c_void,
            retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
            release: Option<extern "C" fn(*const c_void)>,
            copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
        }
        unsafe extern "C" {
            fn CFRunLoopGetMain() -> *mut c_void;
            fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: *const c_void);
            fn CFRunLoopTimerCreate(
                allocator: *mut c_void,
                fire_date: f64,
                interval: f64,
                flags: u64,
                order: CfIndex,
                callout: extern "C" fn(*mut c_void, *mut c_void),
                context: *mut TimerContext,
            ) -> *mut c_void;
            fn CFAbsoluteTimeGetCurrent() -> f64;
            static kCFRunLoopCommonModes: *const c_void;
        }
        extern "C" fn callout(_timer: *mut c_void, info: *mut c_void) {
            // SAFETY: info 由下面 Box::into_raw 造出，只在这里读、只在 Drop 里回收
            //（Drop 先 invalidate，回调不会与回收并发）。
            let f = unsafe { &mut *(info as *mut Box<dyn FnMut()>) };
            f();
        }
        let info = Box::into_raw(Box::new(Box::new(cb) as Box<dyn FnMut()>)) as *mut c_void;
        let mut context = TimerContext {
            version: 0,
            info,
            retain: None,
            release: None,
            copy_description: None,
        };
        let secs = interval.as_secs_f64();
        // SAFETY: 参数均为本函数自造的合法值；kCFRunLoopCommonModes 是 CoreFoundation 导出的
        // CFStringRef（按指针读值）。
        let timer = unsafe {
            let now = CFAbsoluteTimeGetCurrent();
            let timer = CFRunLoopTimerCreate(
                std::ptr::null_mut(),
                now + secs,
                secs,
                0,
                0,
                callout,
                &mut context,
            );
            CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
            timer
        };
        CommonModeTimer { timer, info }
    }

    struct CommonModeTimer {
        timer: *mut std::ffi::c_void,
        info: *mut std::ffi::c_void,
    }

    impl Drop for CommonModeTimer {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn CFRunLoopTimerInvalidate(timer: *mut std::ffi::c_void);
                fn CFRelease(cf: *const std::ffi::c_void);
            }
            unsafe {
                CFRunLoopTimerInvalidate(self.timer);
                CFRelease(self.timer as *const std::ffi::c_void);
                drop(Box::from_raw(self.info as *mut Box<dyn FnMut()>));
            }
        }
    }
}
