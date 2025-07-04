#[cfg(target_os = "linux")]
use super::rdp_input::client::{RdpInputKeyboard, RdpInputMouse};
use super::*;
#[cfg(target_os = "macos")]
use crate::common::is_server;
use crate::input::*;
#[cfg(target_os = "macos")]
use dispatch::Queue;
use enigo::{Enigo, Key, KeyboardControllable, MouseButton, MouseControllable};
use hbb_common::{
    get_time,
    message_proto::{pointer_device_event::Union::TouchEvent, touch_event::Union::ScaleUpdate},
    protobuf::EnumOrUnknown,
};
use rdev::{self, EventType, Key as RdevKey, KeyCode, RawKey};
#[cfg(target_os = "macos")]
use rdev::{CGEventSourceStateID, CGEventTapLocation, VirtualInput};
#[cfg(target_os = "linux")]
use scrap::wayland::pipewire::RDP_SESSION_INFO;
use std::{
    convert::TryFrom,
    ops::{Deref, DerefMut, Sub},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{self, Duration, Instant},
};
#[cfg(windows)]
use winapi::um::winuser::WHEEL_DELTA;

#[cfg(windows)]
extern crate winapi;

// use std::convert::TryInto;
// use serialport;
// use std::env;
// use std::io::{self, Write};
use crate::serial;

use serialport::SerialPort;
use std::fmt::{self, Write};
use std::io;
use std::sync::Mutex;


const INVALID_CURSOR_POS: i32 = i32::MIN;
const INVALID_DISPLAY_IDX: i32 = -1;

#[derive(Default)]
struct StateCursor {
    hcursor: u64,
    cursor_data: Arc<Message>,
    cached_cursor_data: HashMap<u64, Arc<Message>>,
}

impl super::service::Reset for StateCursor {
    fn reset(&mut self) {
        *self = Default::default();
        crate::platform::reset_input_cache();
        fix_key_down_timeout(true);
    }
}

struct StatePos {
    cursor_pos: (i32, i32),
}

impl Default for StatePos {
    fn default() -> Self {
        Self {
            cursor_pos: (INVALID_CURSOR_POS, INVALID_CURSOR_POS),
        }
    }
}

impl super::service::Reset for StatePos {
    fn reset(&mut self) {
        self.cursor_pos = (INVALID_CURSOR_POS, INVALID_CURSOR_POS);
    }
}

impl StatePos {
    #[inline]
    fn is_valid(&self) -> bool {
        self.cursor_pos.0 != INVALID_CURSOR_POS
    }

    #[inline]
    fn is_moved(&self, x: i32, y: i32) -> bool {
        self.is_valid() && (self.cursor_pos.0 != x || self.cursor_pos.1 != y)
    }
}

#[derive(Default)]
struct StateWindowFocus {
    display_idx: i32,
}

impl super::service::Reset for StateWindowFocus {
    fn reset(&mut self) {
        self.display_idx = INVALID_DISPLAY_IDX;
    }
}

impl StateWindowFocus {
    #[inline]
    fn is_valid(&self) -> bool {
        self.display_idx != INVALID_DISPLAY_IDX
    }

    #[inline]
    fn is_changed(&self, disp_idx: i32) -> bool {
        self.is_valid() && self.display_idx != disp_idx
    }
}

#[derive(Default, Clone, Copy)]
struct Input {
    conn: i32,
    time: i64,
    x: i32,
    y: i32,
}

const KEY_CHAR_START: u64 = 9999;

#[derive(Clone, Default)]
pub struct MouseCursorSub {
    inner: ConnInner,
    cached: HashMap<u64, Arc<Message>>,
}

impl From<ConnInner> for MouseCursorSub {
    fn from(inner: ConnInner) -> Self {
        Self {
            inner,
            cached: HashMap::new(),
        }
    }
}

impl Subscriber for MouseCursorSub {
    #[inline]
    fn id(&self) -> i32 {
        self.inner.id()
    }

    #[inline]
    fn send(&mut self, msg: Arc<Message>) {
        if let Some(message::Union::CursorData(cd)) = &msg.union {
            if let Some(msg) = self.cached.get(&cd.id) {
                self.inner.send(msg.clone());
            } else {
                self.inner.send(msg.clone());
                let mut tmp = Message::new();
                // only send id out, require client side cache also
                tmp.set_cursor_id(cd.id);
                self.cached.insert(cd.id, Arc::new(tmp));
            }
        } else {
            self.inner.send(msg);
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
struct LockModesHandler {
    caps_lock_changed: bool,
    num_lock_changed: bool,
}

#[cfg(target_os = "macos")]
struct LockModesHandler;

impl LockModesHandler {
    #[inline]
    fn is_modifier_enabled(key_event: &KeyEvent, modifier: ControlKey) -> bool {
        key_event.modifiers.contains(&modifier.into())
    }

    #[inline]
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn new_handler(key_event: &KeyEvent, _is_numpad_key: bool) -> Self {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            Self::new(key_event, _is_numpad_key)
        }
        #[cfg(target_os = "macos")]
        {
            Self::new(key_event)
        }
    }

    #[cfg(target_os = "linux")]
    fn sleep_to_ensure_locked(v: bool, k: enigo::Key, en: &mut Enigo) {
        if wayland_use_uinput() {
            // Sleep at most 500ms to ensure the lock state is applied.
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if en.get_key_state(k) == v {
                    break;
                }
            }
        } else if wayland_use_rdp_input() {
            // We can't call `en.get_key_state(k)` because there's no api for this.
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn new(key_event: &KeyEvent, is_numpad_key: bool) -> Self {
        let mut en = ENIGO.lock().unwrap();
        let event_caps_enabled = Self::is_modifier_enabled(key_event, ControlKey::CapsLock);
        let local_caps_enabled = en.get_key_state(enigo::Key::CapsLock);
        let caps_lock_changed = event_caps_enabled != local_caps_enabled;
        if caps_lock_changed {
            en.key_click(enigo::Key::CapsLock);
            #[cfg(target_os = "linux")]
            Self::sleep_to_ensure_locked(event_caps_enabled, enigo::Key::CapsLock, &mut en);
        }

        let mut num_lock_changed = false;
        let mut event_num_enabled = false;
        if is_numpad_key {
            let local_num_enabled = en.get_key_state(enigo::Key::NumLock);
            event_num_enabled = Self::is_modifier_enabled(key_event, ControlKey::NumLock);
            num_lock_changed = event_num_enabled != local_num_enabled;
        } else if is_legacy_mode(key_event) {
            #[cfg(target_os = "windows")]
            {
                num_lock_changed =
                    should_disable_numlock(key_event) && en.get_key_state(enigo::Key::NumLock);
            }
        }
        if num_lock_changed {
            en.key_click(enigo::Key::NumLock);
            #[cfg(target_os = "linux")]
            Self::sleep_to_ensure_locked(event_num_enabled, enigo::Key::NumLock, &mut en);
        }

        Self {
            caps_lock_changed,
            num_lock_changed,
        }
    }

    #[cfg(target_os = "macos")]
    fn new(key_event: &KeyEvent) -> Self {
        let event_caps_enabled = Self::is_modifier_enabled(key_event, ControlKey::CapsLock);
        // Do not use the following code to detect `local_caps_enabled`.
        // Because the state of get_key_state will not affect simulation of `VIRTUAL_INPUT_STATE` in this file.
        //
        // let local_caps_enabled = VirtualInput::get_key_state(
        //     CGEventSourceStateID::CombinedSessionState,
        //     rdev::kVK_CapsLock,
        // );
        let local_caps_enabled = unsafe {
            let _lock = VIRTUAL_INPUT_MTX.lock();
            VIRTUAL_INPUT_STATE
                .as_ref()
                .map_or(false, |input| input.capslock_down)
        };
        if event_caps_enabled && !local_caps_enabled {
            press_capslock();
        } else if !event_caps_enabled && local_caps_enabled {
            release_capslock();
        }

        Self {}
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl Drop for LockModesHandler {
    fn drop(&mut self) {
        // Do not change led state if is Wayland uinput.
        // Because there must be a delay to ensure the lock state is applied on Wayland uinput,
        // which may affect the user experience.
        #[cfg(target_os = "linux")]
        if wayland_use_uinput() {
            return;
        }

        let mut en = ENIGO.lock().unwrap();
        if self.caps_lock_changed {
            en.key_click(enigo::Key::CapsLock);
        }
        if self.num_lock_changed {
            en.key_click(enigo::Key::NumLock);
        }
    }
}

#[inline]
#[cfg(target_os = "windows")]
fn should_disable_numlock(evt: &KeyEvent) -> bool {
    // disable numlock if press home etc when numlock is on,
    // because we will get numpad value (7,8,9 etc) if not
    match (&evt.union, evt.mode.enum_value_or(KeyboardMode::Legacy)) {
        (Some(key_event::Union::ControlKey(ck)), KeyboardMode::Legacy) => {
            return NUMPAD_KEY_MAP.contains_key(&ck.value());
        }
        _ => {}
    }
    false
}

pub const NAME_CURSOR: &'static str = "mouse_cursor";
pub const NAME_POS: &'static str = "mouse_pos";
pub const NAME_WINDOW_FOCUS: &'static str = "window_focus";
#[derive(Clone)]
pub struct MouseCursorService {
    pub sp: ServiceTmpl<MouseCursorSub>,
}

impl Deref for MouseCursorService {
    type Target = ServiceTmpl<MouseCursorSub>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for MouseCursorService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

impl MouseCursorService {
    pub fn new(name: String, need_snapshot: bool) -> Self {
        Self {
            sp: ServiceTmpl::<MouseCursorSub>::new(name, need_snapshot),
        }
    }
}

pub fn new_cursor() -> ServiceTmpl<MouseCursorSub> {
    let svc = MouseCursorService::new(NAME_CURSOR.to_owned(), true);
    ServiceTmpl::<MouseCursorSub>::repeat::<StateCursor, _, _>(&svc.clone(), 33, run_cursor);
    svc.sp
}

pub fn new_pos() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME_POS.to_owned(), false);
    GenericService::repeat::<StatePos, _, _>(&svc.clone(), 33, run_pos);
    svc.sp
}

pub fn new_window_focus() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME_WINDOW_FOCUS.to_owned(), false);
    GenericService::repeat::<StateWindowFocus, _, _>(&svc.clone(), 33, run_window_focus);
    svc.sp
}

#[inline]
fn update_last_cursor_pos(x: i32, y: i32) {
    let mut lock = LATEST_SYS_CURSOR_POS.lock().unwrap();
    if lock.1 .0 != x || lock.1 .1 != y {
        (lock.0, lock.1) = (Some(Instant::now()), (x, y))
    }
}

fn run_pos(sp: EmptyExtraFieldService, state: &mut StatePos) -> ResultType<()> {
    let (_, (x, y)) = *LATEST_SYS_CURSOR_POS.lock().unwrap();
    if x == INVALID_CURSOR_POS || y == INVALID_CURSOR_POS {
        return Ok(());
    }

    if state.is_moved(x, y) {
        let mut msg_out = Message::new();
        msg_out.set_cursor_position(CursorPosition {
            x,
            y,
            ..Default::default()
        });
        let exclude = {
            let now = get_time();
            let lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
            if now - lock.time < 300 {
                lock.conn
            } else {
                0
            }
        };
        sp.send_without(msg_out, exclude);
    }
    state.cursor_pos = (x, y);

    sp.snapshot(|sps| {
        let mut msg_out = Message::new();
        msg_out.set_cursor_position(CursorPosition {
            x: state.cursor_pos.0,
            y: state.cursor_pos.1,
            ..Default::default()
        });
        sps.send(msg_out);
        Ok(())
    })?;
    Ok(())
}

fn run_cursor(sp: MouseCursorService, state: &mut StateCursor) -> ResultType<()> {
    if let Some(hcursor) = crate::get_cursor()? {
        if hcursor != state.hcursor {
            let msg;
            if let Some(cached) = state.cached_cursor_data.get(&hcursor) {
                super::log::trace!("Cursor data cached, hcursor: {}", hcursor);
                msg = cached.clone();
            } else {
                let mut data = crate::get_cursor_data(hcursor)?;
                data.colors = hbb_common::compress::compress(&data.colors[..]).into();
                let mut tmp = Message::new();
                tmp.set_cursor_data(data);
                msg = Arc::new(tmp);
                state.cached_cursor_data.insert(hcursor, msg.clone());
                super::log::trace!("Cursor data updated, hcursor: {}", hcursor);
            }
            state.hcursor = hcursor;
            sp.send_shared(msg.clone());
            state.cursor_data = msg;
        }
    }
    sp.snapshot(|sps| {
        sps.send_shared(state.cursor_data.clone());
        Ok(())
    })?;
    Ok(())
}

fn run_window_focus(sp: EmptyExtraFieldService, state: &mut StateWindowFocus) -> ResultType<()> {
    let displays = super::display_service::get_sync_displays();
    if displays.len() <= 1 {
        return Ok(());
    }
    let disp_idx = crate::get_focused_display(displays);
    if let Some(disp_idx) = disp_idx.map(|id| id as i32) {
        if state.is_changed(disp_idx) {
            let mut misc = Misc::new();
            misc.set_follow_current_display(disp_idx as i32);
            let mut msg_out = Message::new();
            msg_out.set_misc(misc);
            sp.send(msg_out);
        }
        state.display_idx = disp_idx;
    }
    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum KeysDown {
    RdevKey(RawKey),
    EnigoKey(u64),
}


lazy_static::lazy_static! {
    static ref PORT: Mutex<Box<dyn serialport::SerialPort>> = {
        let ports = serialport::available_ports().expect("无法获取串口列表");
        if ports.is_empty() {
            panic!("没有可用的串口");
        }
        let port = serialport::new(&ports[0].port_name, 115200)
            .timeout(Duration::from_millis(1000))
            .open()
            .expect("Failed to open serial port");
        Mutex::new(port)
    };
}


struct MouseLast {
    x: i32,
    y: i32,
}

// 上一次鼠标位置
lazy_static::lazy_static! {
    static ref MOUSE_LAST: Mutex<MouseLast> = Mutex::new(MouseLast { x: 0, y: 0 });
}

// 全局鼠标状态
lazy_static::lazy_static! {
    static ref MOUSE_DATA: Mutex<[u8; 4]> = Mutex::new([0; 4]);
}

lazy_static::lazy_static! {
    static ref ENIGO: Arc<Mutex<Enigo>> = {
        Arc::new(Mutex::new(Enigo::new()))
    };
    static ref KEYS_DOWN: Arc<Mutex<HashMap<KeysDown, Instant>>> = Default::default();
    static ref LATEST_PEER_INPUT_CURSOR: Arc<Mutex<Input>> = Default::default();
    static ref LATEST_SYS_CURSOR_POS: Arc<Mutex<(Option<Instant>, (i32, i32))>> = Arc::new(Mutex::new((None, (INVALID_CURSOR_POS, INVALID_CURSOR_POS))));
}
static EXITING: AtomicBool = AtomicBool::new(false);

const MOUSE_MOVE_PROTECTION_TIMEOUT: Duration = Duration::from_millis(1_000);
// Actual diff of (x,y) is (1,1) here. But 5 may be tolerant.
const MOUSE_ACTIVE_DISTANCE: i32 = 5;

static RECORD_CURSOR_POS_RUNNING: AtomicBool = AtomicBool::new(false);

// https://github.com/rustdesk/rustdesk/issues/9729
// We need to do some special handling for macOS when using the legacy mode.
#[cfg(target_os = "macos")]
static LAST_KEY_LEGACY_MODE: AtomicBool = AtomicBool::new(true);
// We use enigo to
// 1. Simulate mouse events
// 2. Simulate the legacy mode key events
// 3. Simulate the functioin key events, like LockScreen
#[inline]
#[cfg(target_os = "macos")]
fn enigo_ignore_flags() -> bool {
    !LAST_KEY_LEGACY_MODE.load(Ordering::SeqCst)
}
#[inline]
#[cfg(target_os = "macos")]
fn set_last_legacy_mode(v: bool) {
    LAST_KEY_LEGACY_MODE.store(v, Ordering::SeqCst);
    ENIGO.lock().unwrap().set_ignore_flags(!v);
}

pub fn try_start_record_cursor_pos() -> Option<thread::JoinHandle<()>> {
    if RECORD_CURSOR_POS_RUNNING.load(Ordering::SeqCst) {
        return None;
    }

    macro_rules! serial_println {
        ($($arg:tt)*) => {
            serial::serial_println(format_args!($($arg)*))
        };
    }

    RECORD_CURSOR_POS_RUNNING.store(true, Ordering::SeqCst);
    let handle = thread::spawn(|| {
        let interval = time::Duration::from_millis(33);
        loop {
            if !RECORD_CURSOR_POS_RUNNING.load(Ordering::SeqCst) {
                break;
            }

            let now = time::Instant::now();
            if let Some((x, y)) = crate::get_cursor_pos() {
                update_last_cursor_pos(x, y);          
            }
    
            let elapsed = now.elapsed();
            if elapsed < interval {
                thread::sleep(interval - elapsed);
            }
        }
        update_last_cursor_pos(INVALID_CURSOR_POS, INVALID_CURSOR_POS);
    });
    Some(handle)
}

pub fn try_stop_record_cursor_pos() {
    let remote_count = AUTHED_CONNS
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.conn_type == AuthConnType::Remote)
        .count();
    if remote_count > 0 {
        return;
    }
    RECORD_CURSOR_POS_RUNNING.store(false, Ordering::SeqCst);
}

// mac key input must be run in main thread, otherwise crash on >= osx 10.15
#[cfg(target_os = "macos")]
lazy_static::lazy_static! {
    static ref QUEUE: Queue = Queue::main();
}

#[cfg(target_os = "macos")]
struct VirtualInputState {
    virtual_input: VirtualInput,
    capslock_down: bool,
}

#[cfg(target_os = "macos")]
impl VirtualInputState {
    fn new() -> Option<Self> {
        VirtualInput::new(
            CGEventSourceStateID::CombinedSessionState,
            // Note: `CGEventTapLocation::Session` will be affected by the mouse events.
            // When we're simulating key events, then move the physical mouse, the key events will be affected.
            // It looks like https://github.com/rustdesk/rustdesk/issues/9729#issuecomment-2432306822
            // 1. Press "Command" key in RustDesk
            // 2. Move the physical mouse
            // 3. Press "V" key in RustDesk
            // Then the controlled side just prints "v" instead of pasting.
            //
            // Changing `CGEventTapLocation::Session` to `CGEventTapLocation::HID` fixes it.
            // But we do not consider this as a bug, because it's not a common case,
            // we consider only RustDesk operates the controlled side.
            //
            // https://developer.apple.com/documentation/coregraphics/cgeventtaplocation/
            CGEventTapLocation::Session,
        )
        .map(|virtual_input| Self {
            virtual_input,
            capslock_down: false,
        })
        .ok()
    }

    #[inline]
    fn simulate(&self, event_type: &EventType) -> ResultType<()> {
        Ok(self.virtual_input.simulate(&event_type)?)
    }
}

#[cfg(target_os = "macos")]
static mut VIRTUAL_INPUT_MTX: Mutex<()> = Mutex::new(());
#[cfg(target_os = "macos")]
static mut VIRTUAL_INPUT_STATE: Option<VirtualInputState> = None;

// First call set_uinput() will create keyboard and mouse clients.
// The clients are ipc connections that must live shorter than tokio runtime.
// Thus this function must not be called in a temporary runtime.
#[cfg(target_os = "linux")]
pub async fn setup_uinput(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    // Keyboard and mouse both open /dev/uinput
    // TODO: Make sure there's no race
    set_uinput_resolution(minx, maxx, miny, maxy).await?;

    let keyboard = super::uinput::client::UInputKeyboard::new().await?;
    log::info!("UInput keyboard created");
    let mouse = super::uinput::client::UInputMouse::new().await?;
    log::info!("UInput mouse created");

    ENIGO
        .lock()
        .unwrap()
        .set_custom_keyboard(Box::new(keyboard));
    ENIGO.lock().unwrap().set_custom_mouse(Box::new(mouse));
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn setup_rdp_input() -> ResultType<(), Box<dyn std::error::Error>> {
    let mut en = ENIGO.lock()?;
    let rdp_info_lock = RDP_SESSION_INFO.lock()?;
    let rdp_info = rdp_info_lock.as_ref().ok_or("RDP session is None")?;

    let keyboard = RdpInputKeyboard::new(rdp_info.conn.clone(), rdp_info.session.clone())?;
    en.set_custom_keyboard(Box::new(keyboard));
    log::info!("RdpInput keyboard created");

    if let Some(stream) = rdp_info.streams.clone().into_iter().next() {
        let resolution = rdp_info
            .resolution
            .lock()
            .unwrap()
            .unwrap_or(stream.get_size());
        let mouse = RdpInputMouse::new(
            rdp_info.conn.clone(),
            rdp_info.session.clone(),
            stream,
            resolution,
        )?;
        en.set_custom_mouse(Box::new(mouse));
        log::info!("RdpInput mouse created");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn update_mouse_resolution(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    set_uinput_resolution(minx, maxx, miny, maxy).await?;

    std::thread::spawn(|| {
        if let Some(mouse) = ENIGO.lock().unwrap().get_custom_mouse() {
            if let Some(mouse) = mouse
                .as_mut_any()
                .downcast_mut::<super::uinput::client::UInputMouse>()
            {
                allow_err!(mouse.send_refresh());
            } else {
                log::error!("failed downcast uinput mouse");
            }
        }
    });

    Ok(())
}

#[cfg(target_os = "linux")]
async fn set_uinput_resolution(minx: i32, maxx: i32, miny: i32, maxy: i32) -> ResultType<()> {
    super::uinput::client::set_resolution(minx, maxx, miny, maxy).await
}

pub fn is_left_up(evt: &MouseEvent) -> bool {
    let buttons = evt.mask >> 3;
    let evt_type = evt.mask & 0x7;
    return buttons == 1 && evt_type == 2;
}

#[cfg(windows)]
pub fn mouse_move_relative(x: i32, y: i32) {
    crate::platform::windows::try_change_desktop();
    let mut en = ENIGO.lock().unwrap();
    en.mouse_move_relative(x, y);
}

#[cfg(windows)]
fn modifier_sleep() {
    // sleep for a while, this is only for keying in rdp in peer so far
    std::thread::sleep(std::time::Duration::from_nanos(1));
}

#[inline]
#[cfg(not(target_os = "macos"))]
fn is_pressed(key: &Key, en: &mut Enigo) -> bool {
    get_modifier_state(key.clone(), en)
}

// Sleep for 8ms is enough in my tests, but we sleep 12ms to be safe.
// sleep 12ms In my test, the characters are already output in real time.
#[inline]
#[cfg(target_os = "macos")]
fn key_sleep() {
    // https://www.reddit.com/r/rustdesk/comments/1kn1w5x/typing_lags_when_connecting_to_macos_clients/
    //
    // There's a strange bug when running by `launchctl load -w /Library/LaunchAgents/abc.plist`
    // `std::thread::sleep(Duration::from_millis(20));` may sleep 90ms or more.
    // Though `/Applications/RustDesk.app/Contents/MacOS/rustdesk --server` in terminal is ok.
    let now = Instant::now();
    while now.elapsed() < Duration::from_millis(12) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[inline]
fn get_modifier_state(key: Key, en: &mut Enigo) -> bool {
    // https://github.com/rustdesk/rustdesk/issues/332
    // on Linux, if RightAlt is down, RightAlt status is false, Alt status is true
    // but on Windows, both are true
    let x = en.get_key_state(key.clone());
    match key {
        Key::Shift => x || en.get_key_state(Key::RightShift),
        Key::Control => x || en.get_key_state(Key::RightControl),
        Key::Alt => x || en.get_key_state(Key::RightAlt),
        Key::Meta => x || en.get_key_state(Key::RWin),
        Key::RightShift => x || en.get_key_state(Key::Shift),
        Key::RightControl => x || en.get_key_state(Key::Control),
        Key::RightAlt => x || en.get_key_state(Key::Alt),
        Key::RWin => x || en.get_key_state(Key::Meta),
        _ => x,
    }
}

pub fn handle_mouse(evt: &MouseEvent, conn: i32) {
    #[cfg(target_os = "macos")]
    {
        // having GUI (--server has tray, it is GUI too), run main GUI thread, otherwise crash
        let evt = evt.clone();
        QUEUE.exec_async(move || handle_mouse_(&evt, conn));
        return;
    }
    #[cfg(windows)]
    crate::portable_service::client::handle_mouse(evt, conn);
    #[cfg(not(windows))]
    handle_mouse_(evt, conn);
}

// to-do: merge handle_mouse and handle_pointer
pub fn handle_pointer(evt: &PointerDeviceEvent, conn: i32) {
    #[cfg(target_os = "macos")]
    {
        // having GUI, run main GUI thread, otherwise crash
        let evt = evt.clone();
        QUEUE.exec_async(move || handle_pointer_(&evt, conn));
        return;
    }
    #[cfg(windows)]
    crate::portable_service::client::handle_pointer(evt, conn);
    #[cfg(not(windows))]
    handle_pointer_(evt, conn);
}

pub fn fix_key_down_timeout_loop() {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(10_000));
        fix_key_down_timeout(false);
    });
    if let Err(err) = ctrlc::set_handler(move || {
        fix_key_down_timeout_at_exit();
        std::process::exit(0); // will call atexit on posix, but not on Windows
    }) {
        log::error!("Failed to set Ctrl-C handler: {}", err);
    }
}

pub fn fix_key_down_timeout_at_exit() {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    EXITING.store(true, Ordering::SeqCst);
    fix_key_down_timeout(true);
    log::info!("fix_key_down_timeout_at_exit");
}

#[inline]
#[cfg(target_os = "linux")]
pub fn clear_remapped_keycode() {
    ENIGO.lock().unwrap().tfc_clear_remapped();
}

#[inline]
fn record_key_is_control_key(record_key: u64) -> bool {
    record_key < KEY_CHAR_START
}

#[inline]
fn record_key_is_chr(record_key: u64) -> bool {
    record_key < KEY_CHAR_START
}

#[inline]
fn record_key_to_key(record_key: u64) -> Option<Key> {
    if record_key_is_control_key(record_key) {
        control_key_value_to_key(record_key as _)
    } else if record_key_is_chr(record_key) {
        let chr: u32 = (record_key - KEY_CHAR_START) as _;
        Some(char_value_to_key(chr))
    } else {
        None
    }
}

pub fn release_device_modifiers() {
    let mut en = ENIGO.lock().unwrap();
    for modifier in [
        Key::Shift,
        Key::Control,
        Key::Alt,
        Key::Meta,
        Key::RightShift,
        Key::RightControl,
        Key::RightAlt,
        Key::RWin,
    ] {
        if get_modifier_state(modifier, &mut en) {
            en.key_up(modifier);
        }
    }
}

#[inline]
fn release_record_key(record_key: KeysDown) {
    let func = move || match record_key {
        KeysDown::RdevKey(raw_key) => {
            simulate_(&EventType::KeyRelease(RdevKey::RawKey(raw_key)));
        }
        KeysDown::EnigoKey(key) => {
            if let Some(key) = record_key_to_key(key) {
                ENIGO.lock().unwrap().key_up(key);
                log::debug!("Fixed {:?} timeout", key);
            }
        }
    };

    #[cfg(target_os = "macos")]
    QUEUE.exec_async(func);
    #[cfg(not(target_os = "macos"))]
    func();
}

fn fix_key_down_timeout(force: bool) {
    let key_down = KEYS_DOWN.lock().unwrap();
    if key_down.is_empty() {
        return;
    }
    let cloned = (*key_down).clone();
    drop(key_down);

    for (record_key, time) in cloned.into_iter() {
        if force || time.elapsed().as_millis() >= 360_000 {
            record_pressed_key(record_key, false);
            release_record_key(record_key);
        }
    }
}

// e.g. current state of ctrl is down, but ctrl not in modifier, we should change ctrl to up, to make modifier state sync between remote and local
#[inline]
fn fix_modifier(
    modifiers: &[EnumOrUnknown<ControlKey>],
    key0: ControlKey,
    key1: Key,
    en: &mut Enigo,
) {
    if get_modifier_state(key1, en) && !modifiers.contains(&EnumOrUnknown::new(key0)) {
        #[cfg(windows)]
        if key0 == ControlKey::Control && get_modifier_state(Key::Alt, en) {
            // AltGr case
            return;
        }
        en.key_up(key1);
        log::debug!("Fixed {:?}", key1);
    }
}

fn fix_modifiers(modifiers: &[EnumOrUnknown<ControlKey>], en: &mut Enigo, ck: i32) {
    if ck != ControlKey::Shift.value() {
        fix_modifier(modifiers, ControlKey::Shift, Key::Shift, en);
    }
    if ck != ControlKey::RShift.value() {
        fix_modifier(modifiers, ControlKey::Shift, Key::RightShift, en);
    }
    if ck != ControlKey::Alt.value() {
        fix_modifier(modifiers, ControlKey::Alt, Key::Alt, en);
    }
    if ck != ControlKey::RAlt.value() {
        fix_modifier(modifiers, ControlKey::Alt, Key::RightAlt, en);
    }
    if ck != ControlKey::Control.value() {
        fix_modifier(modifiers, ControlKey::Control, Key::Control, en);
    }
    if ck != ControlKey::RControl.value() {
        fix_modifier(modifiers, ControlKey::Control, Key::RightControl, en);
    }
    if ck != ControlKey::Meta.value() {
        fix_modifier(modifiers, ControlKey::Meta, Key::Meta, en);
    }
    if ck != ControlKey::RWin.value() {
        fix_modifier(modifiers, ControlKey::Meta, Key::RWin, en);
    }
}

// Update time to avoid send cursor position event to the peer.
// See `run_pos` --> `set_cursor_position` --> `exclude`
#[inline]
pub fn update_latest_input_cursor_time(conn: i32) {
    let mut lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
    lock.conn = conn;
    lock.time = get_time();
}

#[inline]
fn get_last_input_cursor_pos() -> (i32, i32) {
    let lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
    (lock.x, lock.y)
}


// check if mouse is moved by the controlled side user to make controlled side has higher mouse priority than remote.
fn active_mouse_(conn: i32) -> bool {

    true
    /* this method is buggy (not working on macOS, making fast moving mouse event discarded here) and added latency (this is blocking way, must do in async way), so we disable it for now
    // out of time protection
    if LATEST_SYS_CURSOR_POS
        .lock()
        .unwrap()
        .0
        .map(|t| t.elapsed() > MOUSE_MOVE_PROTECTION_TIMEOUT)
        .unwrap_or(true)
    {
        return true;
    }

    // last conn input may be protected
    if LATEST_PEER_INPUT_CURSOR.lock().unwrap().conn != conn {
        return false;
    }

    let in_active_dist = |a: i32, b: i32| -> bool { (a - b).abs() < MOUSE_ACTIVE_DISTANCE };

    // Check if input is in valid range
    match crate::get_cursor_pos() {
        Some((x, y)) => {
            let (last_in_x, last_in_y) = get_last_input_cursor_pos();
            let mut can_active = in_active_dist(last_in_x, x) && in_active_dist(last_in_y, y);
            // The cursor may not have been moved to last input position if system is busy now.
            // While this is not a common case, we check it again after some time later.
            if !can_active {
                // 100 micros may be enough for system to move cursor.
                // Mouse inputs on macOS are asynchronous. 1. Put in a queue to process in main thread. 2. Send event async.
                // More reties are needed on macOS.
                #[cfg(not(target_os = "macos"))]
                let retries = 10;
                #[cfg(target_os = "macos")]
                let retries = 100;
                #[cfg(not(target_os = "macos"))]
                let sleep_interval: u64 = 10;
                #[cfg(target_os = "macos")]
                let sleep_interval: u64 = 30;
                for _retry in 0..retries {
                    std::thread::sleep(std::time::Duration::from_micros(sleep_interval));
                    // Sleep here can also somehow suppress delay accumulation.
                    if let Some((x2, y2)) = crate::get_cursor_pos() {
                        let (last_in_x, last_in_y) = get_last_input_cursor_pos();
                        can_active = in_active_dist(last_in_x, x2) && in_active_dist(last_in_y, y2);
                        if can_active {
                            break;
                        }
                    }
                }
            }
            if !can_active {
                let mut lock = LATEST_PEER_INPUT_CURSOR.lock().unwrap();
                lock.x = INVALID_CURSOR_POS / 2;
                lock.y = INVALID_CURSOR_POS / 2;
            }
            can_active
        }
        None => true,
    }
    */
}

pub fn handle_pointer_(evt: &PointerDeviceEvent, conn: i32) {
    if !active_mouse_(conn) {
        return;
    }

    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    match &evt.union {
        Some(TouchEvent(evt)) => match &evt.union {
            Some(ScaleUpdate(_scale_evt)) => {
                #[cfg(target_os = "windows")]
                handle_scale(_scale_evt.scale);
            }
            _ => {}
        },
        _ => {}
    }
}

pub fn build_frame(custom_bytes: [u8; 4]) -> [u8; 6] {
    let mut frame = [0u8; 6];
    frame[0] = 0x5A; // 固定头字节
    frame[1..5].copy_from_slice(&custom_bytes); // 设置第2-5字节
    frame[5] = frame.iter().take(5).fold(0u8, |sum, &x| sum.wrapping_add(x)); // 计算校验和
    frame
}

/// 使用全局串口发送数据帧
pub fn send_frame(frame: &[u8]) -> io::Result<()> {
    let mut port = PORT.lock().unwrap();
    port.write_all(frame)?;
    port.flush()?;
    //println!("已发送数据: {:02X?}", frame);
    Ok(())
}

pub fn get_cursor_pos_dokin() -> Option<(i32, i32)> {
    use winapi::shared::windef::POINT;
    use winapi::um::winuser::GetCursorPos;

    let mut pt = POINT { x: -1, y: -1 };
    let ret = unsafe { GetCursorPos(&mut pt) };
    if ret != 1 || pt.x == -1 && pt.y == -1 {
        None
    } else {
        Some((pt.x, pt.y))
    }
}

pub fn handle_mouse_(evt: &MouseEvent, conn: i32) {
    if !active_mouse_(conn) {
        return;
    }

    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();
    let buttons = evt.mask >> 3;
    let evt_type = evt.mask & 0x7;
    let mut en = ENIGO.lock().unwrap();
    #[cfg(target_os = "macos")]
    en.set_ignore_flags(enigo_ignore_flags());
    #[cfg(not(target_os = "macos"))]
    let mut to_release = Vec::new();
    if evt_type == MOUSE_TYPE_DOWN {
        fix_modifiers(&evt.modifiers[..], &mut en, 0);
        #[cfg(target_os = "macos")]
        en.reset_flag();
        for ref ck in evt.modifiers.iter() {
            if let Some(key) = KEY_MAP.get(&ck.value()) {
                #[cfg(target_os = "macos")]
                en.add_flag(key);
                #[cfg(not(target_os = "macos"))]
                if key != &Key::CapsLock && key != &Key::NumLock {
                    if !get_modifier_state(key.clone(), &mut en) {
                        //en.key_down(key.clone()).ok();    //dokin
                        #[cfg(windows)]
                        modifier_sleep();
                        to_release.push(key);
                    }
                }
            }
        }
    }

    // 定义局部宏简化调用（可选）
    macro_rules! serial_println {
        ($($arg:tt)*) => {
            serial::serial_println(format_args!($($arg)*))
        };
    }

    match evt_type {
    	MOUSE_TYPE_MOVE => {
            
            let (_, (x, y)) = *LATEST_SYS_CURSOR_POS.lock().unwrap();

            let xx = evt.x - x;
            let yy = evt.y - y;

            if xx > 127 || xx < -128 || yy > 127 || yy < -128 {
                en.mouse_move_to(evt.x, evt.y);
                serial_println!("evt1({},{})\r\n", evt.x, evt.y);
            }
            else{
                let delta_x = if evt.x > x {
                    (evt.x - x).min(127) // 限制最大差值
                } else {
                    (x - evt.x).min(127) * -1
                };
    
                let delta_y = if evt.y > y {
                    (evt.y - y).min(127) // 限制最大差值
                } else {
                    (y - evt.y).min(127) * -1
                };
                if delta_x != 0 && delta_y != 0{
    
                    let mut en = ENIGO.lock().unwrap();
                    en.mouse_move_relative(delta_x, delta_y);
                    serial_println!("evt({},{}), cur({},{}), del({},{})\r\n", evt.x, evt.y, x, y, delta_x, delta_y);
                }
            }

            *LATEST_PEER_INPUT_CURSOR.lock().unwrap() = Input {
                conn,
                time: get_time(),
                x: evt.x,
                y: evt.y,
            };

        }
        MOUSE_TYPE_DOWN => match buttons {
            MOUSE_BUTTON_LEFT => {
                // log::info!("MOUSE_BUTTON_LEFT down");
                allow_err!(en.mouse_down(MouseButton::Left));
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] | 0x01;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);
            }
            MOUSE_BUTTON_RIGHT => {
                allow_err!(en.mouse_down(MouseButton::Right));
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] | 0x02;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);			
            }
            MOUSE_BUTTON_WHEEL => {
                allow_err!(en.mouse_down(MouseButton::Middle));
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] | 0x04;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);				
            }
            MOUSE_BUTTON_BACK => {
                allow_err!(en.mouse_down(MouseButton::Back));
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] | 0x08;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);		                
            }
            MOUSE_BUTTON_FORWARD => {
                allow_err!(en.mouse_down(MouseButton::Forward));
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] | 0x10;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);		                
            }
            _ => {}
        },
        MOUSE_TYPE_UP => match buttons {
            MOUSE_BUTTON_LEFT => {
                en.mouse_up(MouseButton::Left);
			// 	let mut mouse_data = MOUSE_DATA.lock().unwrap();
			// 	mouse_data[0] = mouse_data[0] & 0xFE;
            //     mouse_data[1] = 0;
            //     mouse_data[2] = 0;
			// 	let frame = build_frame(*mouse_data);
			// 	let _ = send_frame(&frame);
            }
            MOUSE_BUTTON_RIGHT => {
                en.mouse_up(MouseButton::Right);
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] & 0xFD;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);				
            }
            MOUSE_BUTTON_WHEEL => {
                en.mouse_up(MouseButton::Middle);
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] & 0xFB;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);
            }
            MOUSE_BUTTON_BACK => {
                en.mouse_up(MouseButton::Back);
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] & 0xF7;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);                
            }
            MOUSE_BUTTON_FORWARD => {
                en.mouse_up(MouseButton::Forward);
				// let mut mouse_data = MOUSE_DATA.lock().unwrap();
				// mouse_data[0] = mouse_data[0] & 0xEF;
                // mouse_data[1] = 0;
                // mouse_data[2] = 0;
				// let frame = build_frame(*mouse_data);
				// let _ = send_frame(&frame);                
            }
            _ => {}
        },
        MOUSE_TYPE_WHEEL | MOUSE_TYPE_TRACKPAD => {
            #[allow(unused_mut)]
            let mut x = -evt.x;
            #[allow(unused_mut)]
            let mut y = evt.y;
            #[cfg(not(windows))]
            {
                y = -y;
            }

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let is_track_pad = evt_type == MOUSE_TYPE_TRACKPAD;

            #[cfg(target_os = "macos")]
            {
                // TODO: support track pad on win.

                // fix shift + scroll(down/up)
                if !is_track_pad
                    && evt
                        .modifiers
                        .contains(&EnumOrUnknown::new(ControlKey::Shift))
                {
                    x = y;
                    y = 0;
                }

                if x != 0 {
                    en.mouse_scroll_x(x, is_track_pad);
                }
                if y != 0 {
                    en.mouse_scroll_y(y, is_track_pad);
                }
            }

            #[cfg(windows)]
            if !is_track_pad {
                x *= WHEEL_DELTA as i32;
                y *= WHEEL_DELTA as i32;
            }

            #[cfg(not(target_os = "macos"))]
            {
                if y != 0 {
                    en.mouse_scroll_y(y);
					// let mut mouse_data = MOUSE_DATA.lock().unwrap();
                    // if y >= 0 && y < 128 {
                    //     mouse_data[3] = y.try_into().unwrap_or(0);
                    // }
                    // else if y >= -128 && y < 0 {
                    //     mouse_data[3] = (y+256).try_into().unwrap_or(0);
                    // }
                    // else{
                    //     mouse_data[3] = 0;
                    // }
                    // mouse_data[1] = 0;
                    // mouse_data[2] = 0;                    
					    
					// let frame = build_frame(*mouse_data);
					// let _ = send_frame(&frame);
                }
                if x != 0 {
                    en.mouse_scroll_x(x);
                }
            }
        }
        _ => {}
    }
    #[cfg(not(target_os = "macos"))]
    for key in to_release {
        en.key_up(key.clone());   //dokin
    }
}

#[cfg(target_os = "windows")]
fn handle_scale(scale: i32) {
    let mut en = ENIGO.lock().unwrap();
    if scale == 0 {
        en.key_up(Key::Control);
    } else {
        if en.key_down(Key::Control).is_ok() {
            en.mouse_scroll_y(scale);
        }
    }
}

pub fn is_enter(evt: &KeyEvent) -> bool {
    if let Some(key_event::Union::ControlKey(ck)) = evt.union {
        if ck.value() == ControlKey::Return.value() || ck.value() == ControlKey::NumpadEnter.value()
        {
            return true;
        }
    }
    return false;
}

pub async fn lock_screen() {
    cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        // xdg_screensaver lock not work on Linux from our service somehow
        // loginctl lock-session also not work, they both work run rustdesk from cmd
        std::thread::spawn(|| {
            let mut key_event = KeyEvent::new();

            key_event.set_chr('l' as _);
            key_event.modifiers.push(ControlKey::Meta.into());
            key_event.mode = KeyboardMode::Legacy.into();

            key_event.down = true;
            handle_key(&key_event);

            key_event.down = false;
            handle_key(&key_event);
        });
    } else if #[cfg(target_os = "macos")] {
        // CGSession -suspend not real lock screen, it is user switch
        std::thread::spawn(|| {
            let mut key_event = KeyEvent::new();

            key_event.set_chr('q' as _);
            key_event.modifiers.push(ControlKey::Meta.into());
            key_event.modifiers.push(ControlKey::Control.into());
            key_event.mode = KeyboardMode::Legacy.into();

            key_event.down = true;
            handle_key(&key_event);
            key_event.down = false;
            handle_key(&key_event);
        });
    } else {
    crate::platform::lock_screen();
    }
    }
}

#[inline]
#[cfg(target_os = "linux")]
pub fn handle_key(evt: &KeyEvent) {
    handle_key_(evt);
}

#[inline]
#[cfg(target_os = "windows")]
pub fn handle_key(evt: &KeyEvent) {
    crate::portable_service::client::handle_key(evt);
}

#[inline]
#[cfg(target_os = "macos")]
pub fn handle_key(evt: &KeyEvent) {
    // having GUI, run main GUI thread, otherwise crash
    let evt = evt.clone();
    QUEUE.exec_async(move || handle_key_(&evt));
    // Key sleep is required for macOS.
    // If we don't sleep, the key press/release events may not take effect.
    //
    // For example, the controlled side osx `12.7.6` or `15.1.1`
    // If we input characters quickly and continuously, and press or release "Shift" for a short period of time,
    // it is possible that after releasing "Shift", the controlled side will still print uppercase characters.
    // Though it is not very easy to reproduce.
    key_sleep();
}

#[cfg(target_os = "macos")]
#[inline]
fn reset_input() {
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        VIRTUAL_INPUT_STATE = VirtualInputState::new();
    }
}

#[cfg(target_os = "macos")]
pub fn reset_input_ondisconn() {
    QUEUE.exec_async(reset_input);
}

fn sim_rdev_rawkey_position(code: KeyCode, keydown: bool) {
    #[cfg(target_os = "windows")]
    let rawkey = RawKey::ScanCode(code);
    #[cfg(target_os = "linux")]
    let rawkey = RawKey::LinuxXorgKeycode(code);
    // // to-do: test android
    // #[cfg(target_os = "android")]
    // let rawkey = RawKey::LinuxConsoleKeycode(code);
    #[cfg(target_os = "macos")]
    let rawkey = RawKey::MacVirtualKeycode(code);

    // map mode(1): Send keycode according to the peer platform.
    record_pressed_key(KeysDown::RdevKey(rawkey), keydown);

    let event_type = if keydown {
        EventType::KeyPress(RdevKey::RawKey(rawkey))
    } else {
        EventType::KeyRelease(RdevKey::RawKey(rawkey))
    };
    simulate_(&event_type);
}

#[cfg(target_os = "windows")]
fn sim_rdev_rawkey_virtual(code: u32, keydown: bool) {
    let rawkey = RawKey::WinVirtualKeycode(code);
    record_pressed_key(KeysDown::RdevKey(rawkey), keydown);
    let event_type = if keydown {
        EventType::KeyPress(RdevKey::RawKey(rawkey))
    } else {
        EventType::KeyRelease(RdevKey::RawKey(rawkey))
    };
    simulate_(&event_type);
}

#[inline]
#[cfg(target_os = "macos")]
fn simulate_(event_type: &EventType) {
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = &VIRTUAL_INPUT_STATE {
            let _ = input.simulate(&event_type);
        }
    }
}

#[inline]
#[cfg(target_os = "macos")]
fn press_capslock() {
    let caps_key = RdevKey::RawKey(rdev::RawKey::MacVirtualKeycode(rdev::kVK_CapsLock));
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = &mut VIRTUAL_INPUT_STATE {
            if input.simulate(&EventType::KeyPress(caps_key)).is_ok() {
                input.capslock_down = true;
                key_sleep();
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[inline]
fn release_capslock() {
    let caps_key = RdevKey::RawKey(rdev::RawKey::MacVirtualKeycode(rdev::kVK_CapsLock));
    unsafe {
        let _lock = VIRTUAL_INPUT_MTX.lock();
        if let Some(input) = &mut VIRTUAL_INPUT_STATE {
            if input.simulate(&EventType::KeyRelease(caps_key)).is_ok() {
                input.capslock_down = false;
                key_sleep();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[inline]
fn simulate_(event_type: &EventType) {
    match rdev::simulate(&event_type) {
        Ok(()) => (),
        Err(_simulate_error) => {
            log::error!("Could not send {:?}", &event_type);
        }
    }
}

#[inline]
fn control_key_value_to_key(value: i32) -> Option<Key> {
    KEY_MAP.get(&value).and_then(|k| Some(*k))
}

#[inline]
fn char_value_to_key(value: u32) -> Key {
    Key::Layout(std::char::from_u32(value).unwrap_or('\0'))
}

fn map_keyboard_mode(evt: &KeyEvent) {
    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();

    // Wayland
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() {
        let mut en = ENIGO.lock().unwrap();
        let code = evt.chr() as u16;

        if evt.down {
            en.key_down(enigo::Key::Raw(code)).ok();
        } else {
            en.key_up(enigo::Key::Raw(code));
        }
        return;
    }

    sim_rdev_rawkey_position(evt.chr() as _, evt.down);
}

#[cfg(target_os = "macos")]
fn add_flags_to_enigo(en: &mut Enigo, key_event: &KeyEvent) {
    // When long-pressed the command key, then press and release
    // the Tab key, there should be CGEventFlagCommand in the flag.
    en.reset_flag();
    for ck in key_event.modifiers.iter() {
        if let Some(key) = KEY_MAP.get(&ck.value()) {
            en.add_flag(key);
        }
    }
}

fn get_control_key_value(key_event: &KeyEvent) -> i32 {
    if let Some(key_event::Union::ControlKey(ck)) = key_event.union {
        ck.value()
    } else {
        -1
    }
}

fn release_unpressed_modifiers(en: &mut Enigo, key_event: &KeyEvent) {
    let ck_value = get_control_key_value(key_event);
    fix_modifiers(&key_event.modifiers[..], en, ck_value);
}

#[cfg(target_os = "linux")]
fn is_altgr_pressed() -> bool {
    let altgr_rawkey = RawKey::LinuxXorgKeycode(ControlKey::RAlt.value() as _);
    KEYS_DOWN
        .lock()
        .unwrap()
        .get(&KeysDown::RdevKey(altgr_rawkey))
        .is_some()
}

#[cfg(not(target_os = "macos"))]
fn press_modifiers(en: &mut Enigo, key_event: &KeyEvent, to_release: &mut Vec<Key>) {
    for ref ck in key_event.modifiers.iter() {
        if let Some(key) = control_key_value_to_key(ck.value()) {
            if !is_pressed(&key, en) {
                #[cfg(target_os = "linux")]
                if key == Key::Alt && is_altgr_pressed() {
                    continue;
                }
                en.key_down(key.clone()).ok();
                to_release.push(key.clone());
                #[cfg(windows)]
                modifier_sleep();
            }
        }
    }
}

fn sync_modifiers(en: &mut Enigo, key_event: &KeyEvent, _to_release: &mut Vec<Key>) {
    #[cfg(target_os = "macos")]
    add_flags_to_enigo(en, key_event);

    if key_event.down {
        release_unpressed_modifiers(en, key_event);
        #[cfg(not(target_os = "macos"))]
        press_modifiers(en, key_event, _to_release);
    }
}

fn process_control_key(en: &mut Enigo, ck: &EnumOrUnknown<ControlKey>, down: bool) {
    if let Some(key) = control_key_value_to_key(ck.value()) {
        if down {
            en.key_down(key).ok();
        } else {
            en.key_up(key);
        }
    }
}

#[inline]
fn need_to_uppercase(en: &mut Enigo) -> bool {
    get_modifier_state(Key::Shift, en) || get_modifier_state(Key::CapsLock, en)
}

fn process_chr(en: &mut Enigo, chr: u32, down: bool) {
    let key = char_value_to_key(chr);

    if down {
        if en.key_down(key).is_ok() {
        } else {
            if let Ok(chr) = char::try_from(chr) {
                let mut s = chr.to_string();
                if need_to_uppercase(en) {
                    s = s.to_uppercase();
                }
                en.key_sequence(&s);
            };
        }
    } else {
        en.key_up(key);
    }
}

fn process_unicode(en: &mut Enigo, chr: u32) {
    if let Ok(chr) = char::try_from(chr) {
        en.key_sequence(&chr.to_string());
    }
}

fn process_seq(en: &mut Enigo, sequence: &str) {
    en.key_sequence(&sequence);
}

#[cfg(not(target_os = "macos"))]
fn release_keys(en: &mut Enigo, to_release: &Vec<Key>) {
    for key in to_release {
        en.key_up(key.clone());
    }
}

fn record_pressed_key(record_key: KeysDown, down: bool) {
    let mut key_down = KEYS_DOWN.lock().unwrap();
    if down {
        key_down.insert(record_key, Instant::now());
    } else {
        key_down.remove(&record_key);
    }
}

fn is_function_key(ck: &EnumOrUnknown<ControlKey>) -> bool {
    let mut res = false;
    if ck.value() == ControlKey::CtrlAltDel.value() {
        // have to spawn new thread because send_sas is tokio_main, the caller can not be tokio_main.
        #[cfg(windows)]
        std::thread::spawn(|| {
            allow_err!(send_sas());
        });
        res = true;
    } else if ck.value() == ControlKey::LockScreen.value() {
        std::thread::spawn(|| {
            lock_screen_2();
        });
        res = true;
    }
    return res;
}

fn legacy_keyboard_mode(evt: &KeyEvent) {
    #[cfg(windows)]
    crate::platform::windows::try_change_desktop();
    let mut to_release: Vec<Key> = Vec::new();

    let mut en = ENIGO.lock().unwrap();
    sync_modifiers(&mut en, &evt, &mut to_release);

    let down = evt.down;
    match evt.union {
        Some(key_event::Union::ControlKey(ck)) => {
            if is_function_key(&ck) {
                return;
            }
            let record_key = ck.value() as u64;
            record_pressed_key(KeysDown::EnigoKey(record_key), down);
            process_control_key(&mut en, &ck, down)
        }
        Some(key_event::Union::Chr(chr)) => {
            let record_key = chr as u64 + KEY_CHAR_START;
            record_pressed_key(KeysDown::EnigoKey(record_key), down);
            process_chr(&mut en, chr, down)
        }
        Some(key_event::Union::Unicode(chr)) => process_unicode(&mut en, chr),
        Some(key_event::Union::Seq(ref seq)) => process_seq(&mut en, seq),
        _ => {}
    }

    #[cfg(not(target_os = "macos"))]
    release_keys(&mut en, &to_release);
}

#[cfg(target_os = "windows")]
fn translate_process_code(code: u32, down: bool) {
    crate::platform::windows::try_change_desktop();
    match code >> 16 {
        0 => sim_rdev_rawkey_position(code as _, down),
        vk_code => sim_rdev_rawkey_virtual(vk_code, down),
    };
}

fn translate_keyboard_mode(evt: &KeyEvent) {
    match &evt.union {
        Some(key_event::Union::Seq(seq)) => {
            // Fr -> US
            // client: Shift + & => 1(send to remote)
            // remote: Shift + 1 => !
            //
            // Try to release shift first.
            // remote: Shift + 1 => 1
            let mut en = ENIGO.lock().unwrap();

            #[cfg(target_os = "macos")]
            en.key_sequence(seq);
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                #[cfg(target_os = "windows")]
                let simulate_win_hot_key = is_hot_key_modifiers_down(&mut en);
                #[cfg(target_os = "linux")]
                let simulate_win_hot_key = false;
                if !simulate_win_hot_key {
                    if get_modifier_state(Key::Shift, &mut en) {
                        simulate_(&EventType::KeyRelease(RdevKey::ShiftLeft));
                    }
                    if get_modifier_state(Key::RightShift, &mut en) {
                        simulate_(&EventType::KeyRelease(RdevKey::ShiftRight));
                    }
                }
                for chr in seq.chars() {
                    // char in rust is 4 bytes.
                    // But for this case, char comes from keyboard. We only need 2 bytes.
                    #[cfg(target_os = "windows")]
                    if simulate_win_hot_key {
                        rdev::simulate_char(chr, true).ok();
                    } else {
                        rdev::simulate_unicode(chr as _).ok();
                    }
                    #[cfg(target_os = "linux")]
                    en.key_click(Key::Layout(chr));
                }
            }
        }
        Some(key_event::Union::Chr(..)) => {
            #[cfg(target_os = "windows")]
            translate_process_code(evt.chr(), evt.down);
            #[cfg(not(target_os = "windows"))]
            sim_rdev_rawkey_position(evt.chr() as _, evt.down);
        }
        Some(key_event::Union::Unicode(..)) => {
            // Do not handle unicode for now.
        }
        #[cfg(target_os = "windows")]
        Some(key_event::Union::Win2winHotkey(code)) => {
            simulate_win2win_hotkey(*code, evt.down);
        }
        _ => {
            log::debug!("Unreachable. Unexpected key event {:?}", &evt);
        }
    }
}

#[inline]
#[cfg(target_os = "windows")]
fn is_hot_key_modifiers_down(en: &mut Enigo) -> bool {
    en.get_key_state(Key::Control)
        || en.get_key_state(Key::RightControl)
        || en.get_key_state(Key::Alt)
        || en.get_key_state(Key::RightAlt)
        || en.get_key_state(Key::Meta)
        || en.get_key_state(Key::RWin)
}

#[cfg(target_os = "windows")]
fn simulate_win2win_hotkey(code: u32, down: bool) {
    let unicode: u16 = (code & 0x0000FFFF) as u16;
    if down {
        if rdev::simulate_key_unicode(unicode, false).is_ok() {
            return;
        }
    }

    let keycode: u16 = ((code >> 16) & 0x0000FFFF) as u16;
    let scan = rdev::vk_to_scancode(keycode as _);
    allow_err!(rdev::simulate_code(None, Some(scan), down));
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn skip_led_sync_control_key(_key: &ControlKey) -> bool {
    false
}

// LockModesHandler should not be created when single meta is pressing and releasing.
// Because the drop function may insert "CapsLock Click" and "NumLock Click", which breaks single meta click.
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1496936687
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1500415822
// https://github.com/rustdesk/rustdesk/issues/3928#issuecomment-1500773473
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn skip_led_sync_control_key(key: &ControlKey) -> bool {
    matches!(
        key,
        ControlKey::Control
            | ControlKey::RControl
            | ControlKey::Meta
            | ControlKey::Shift
            | ControlKey::RShift
            | ControlKey::Alt
            | ControlKey::RAlt
            | ControlKey::Tab
            | ControlKey::Return
    )
}

#[inline]
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn is_numpad_control_key(key: &ControlKey) -> bool {
    matches!(
        key,
        ControlKey::Numpad0
            | ControlKey::Numpad1
            | ControlKey::Numpad2
            | ControlKey::Numpad3
            | ControlKey::Numpad4
            | ControlKey::Numpad5
            | ControlKey::Numpad6
            | ControlKey::Numpad7
            | ControlKey::Numpad8
            | ControlKey::Numpad9
            | ControlKey::NumpadEnter
    )
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn skip_led_sync_rdev_key(_key: &RdevKey) -> bool {
    false
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn skip_led_sync_rdev_key(key: &RdevKey) -> bool {
    matches!(
        key,
        RdevKey::ControlLeft
            | RdevKey::ControlRight
            | RdevKey::MetaLeft
            | RdevKey::MetaRight
            | RdevKey::ShiftLeft
            | RdevKey::ShiftRight
            | RdevKey::Alt
            | RdevKey::AltGr
            | RdevKey::Tab
            | RdevKey::Return
    )
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_legacy_mode(evt: &KeyEvent) -> bool {
    evt.mode.enum_value_or(KeyboardMode::Legacy) == KeyboardMode::Legacy
}

pub fn handle_key_(evt: &KeyEvent) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let mut _lock_mode_handler = None;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    match &evt.union {
        Some(key_event::Union::Unicode(..)) | Some(key_event::Union::Seq(..)) => {
            _lock_mode_handler = Some(LockModesHandler::new_handler(&evt, false));
        }
        Some(key_event::Union::ControlKey(ck)) => {
            let key = ck.enum_value_or(ControlKey::Unknown);
            if !skip_led_sync_control_key(&key) {
                #[cfg(target_os = "macos")]
                let is_numpad_key = false;
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                let is_numpad_key = is_numpad_control_key(&key);
                _lock_mode_handler = Some(LockModesHandler::new_handler(&evt, is_numpad_key));
            }
        }
        Some(key_event::Union::Chr(code)) => {
            if is_legacy_mode(&evt) {
                _lock_mode_handler = Some(LockModesHandler::new_handler(evt, false));
            } else {
                let key = crate::keyboard::keycode_to_rdev_key(*code);
                if !skip_led_sync_rdev_key(&key) {
                    #[cfg(target_os = "macos")]
                    let is_numpad_key = false;
                    #[cfg(any(target_os = "windows", target_os = "linux"))]
                    let is_numpad_key = crate::keyboard::is_numpad_rdev_key(&key);
                    _lock_mode_handler = Some(LockModesHandler::new_handler(evt, is_numpad_key));
                }
            }
        }
        _ => {}
    }

    match evt.mode.enum_value() {
        Ok(KeyboardMode::Map) => {
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(false);
            map_keyboard_mode(evt);
        }
        Ok(KeyboardMode::Translate) => {
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(false);
            translate_keyboard_mode(evt);
        }
        _ => {
            // All key down events are started from here,
            // so we can reset the flag of last legacy mode here.
            #[cfg(target_os = "macos")]
            set_last_legacy_mode(true);
            legacy_keyboard_mode(evt);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn lock_screen_2() {
    lock_screen().await;
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn send_sas() -> ResultType<()> {
    if crate::platform::is_physical_console_session().unwrap_or(true) {
        let mut stream = crate::ipc::connect(1000, crate::POSTFIX_SERVICE).await?;
        timeout(1000, stream.send(&crate::ipc::Data::SAS)).await??;
    } else {
        crate::platform::send_sas();
    };
    Ok(())
}

#[inline]
#[cfg(target_os = "linux")]
pub fn wayland_use_uinput() -> bool {
    !crate::platform::is_x11() && crate::is_server()
}

#[inline]
#[cfg(target_os = "linux")]
pub fn wayland_use_rdp_input() -> bool {
    !crate::platform::is_x11() && !crate::is_server()
}

lazy_static::lazy_static! {
    static ref MODIFIER_MAP: HashMap<i32, Key> = [
        (ControlKey::Alt, Key::Alt),
        (ControlKey::RAlt, Key::RightAlt),
        (ControlKey::Control, Key::Control),
        (ControlKey::RControl, Key::RightControl),
        (ControlKey::Shift, Key::Shift),
        (ControlKey::RShift, Key::RightShift),
        (ControlKey::Meta, Key::Meta),
        (ControlKey::RWin, Key::RWin),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
    static ref KEY_MAP: HashMap<i32, Key> =
    [
        (ControlKey::Alt, Key::Alt),
        (ControlKey::Backspace, Key::Backspace),
        (ControlKey::CapsLock, Key::CapsLock),
        (ControlKey::Control, Key::Control),
        (ControlKey::Delete, Key::Delete),
        (ControlKey::DownArrow, Key::DownArrow),
        (ControlKey::End, Key::End),
        (ControlKey::Escape, Key::Escape),
        (ControlKey::F1, Key::F1),
        (ControlKey::F10, Key::F10),
        (ControlKey::F11, Key::F11),
        (ControlKey::F12, Key::F12),
        (ControlKey::F2, Key::F2),
        (ControlKey::F3, Key::F3),
        (ControlKey::F4, Key::F4),
        (ControlKey::F5, Key::F5),
        (ControlKey::F6, Key::F6),
        (ControlKey::F7, Key::F7),
        (ControlKey::F8, Key::F8),
        (ControlKey::F9, Key::F9),
        (ControlKey::Home, Key::Home),
        (ControlKey::LeftArrow, Key::LeftArrow),
        (ControlKey::Meta, Key::Meta),
        (ControlKey::Option, Key::Option),
        (ControlKey::PageDown, Key::PageDown),
        (ControlKey::PageUp, Key::PageUp),
        (ControlKey::Return, Key::Return),
        (ControlKey::RightArrow, Key::RightArrow),
        (ControlKey::Shift, Key::Shift),
        (ControlKey::Space, Key::Space),
        (ControlKey::Tab, Key::Tab),
        (ControlKey::UpArrow, Key::UpArrow),
        (ControlKey::Numpad0, Key::Numpad0),
        (ControlKey::Numpad1, Key::Numpad1),
        (ControlKey::Numpad2, Key::Numpad2),
        (ControlKey::Numpad3, Key::Numpad3),
        (ControlKey::Numpad4, Key::Numpad4),
        (ControlKey::Numpad5, Key::Numpad5),
        (ControlKey::Numpad6, Key::Numpad6),
        (ControlKey::Numpad7, Key::Numpad7),
        (ControlKey::Numpad8, Key::Numpad8),
        (ControlKey::Numpad9, Key::Numpad9),
        (ControlKey::Cancel, Key::Cancel),
        (ControlKey::Clear, Key::Clear),
        (ControlKey::Menu, Key::Alt),
        (ControlKey::Pause, Key::Pause),
        (ControlKey::Kana, Key::Kana),
        (ControlKey::Hangul, Key::Hangul),
        (ControlKey::Junja, Key::Junja),
        (ControlKey::Final, Key::Final),
        (ControlKey::Hanja, Key::Hanja),
        (ControlKey::Kanji, Key::Kanji),
        (ControlKey::Convert, Key::Convert),
        (ControlKey::Select, Key::Select),
        (ControlKey::Print, Key::Print),
        (ControlKey::Execute, Key::Execute),
        (ControlKey::Snapshot, Key::Snapshot),
        (ControlKey::Insert, Key::Insert),
        (ControlKey::Help, Key::Help),
        (ControlKey::Sleep, Key::Sleep),
        (ControlKey::Separator, Key::Separator),
        (ControlKey::Scroll, Key::Scroll),
        (ControlKey::NumLock, Key::NumLock),
        (ControlKey::RWin, Key::RWin),
        (ControlKey::Apps, Key::Apps),
        (ControlKey::Multiply, Key::Multiply),
        (ControlKey::Add, Key::Add),
        (ControlKey::Subtract, Key::Subtract),
        (ControlKey::Decimal, Key::Decimal),
        (ControlKey::Divide, Key::Divide),
        (ControlKey::Equals, Key::Equals),
        (ControlKey::NumpadEnter, Key::NumpadEnter),
        (ControlKey::RAlt, Key::RightAlt),
        (ControlKey::RControl, Key::RightControl),
        (ControlKey::RShift, Key::RightShift),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
    static ref NUMPAD_KEY_MAP: HashMap<i32, bool> =
    [
        (ControlKey::Home, true),
        (ControlKey::UpArrow, true),
        (ControlKey::PageUp, true),
        (ControlKey::LeftArrow, true),
        (ControlKey::RightArrow, true),
        (ControlKey::End, true),
        (ControlKey::DownArrow, true),
        (ControlKey::PageDown, true),
        (ControlKey::Insert, true),
        (ControlKey::Delete, true),
    ].iter().map(|(a, b)| (a.value(), b.clone())).collect();
}
