use super::{Error, Result, abi};
use std::{
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

#[derive(Clone, Copy)]
pub(crate) struct Api {
    pub sys_init: abi::Init,
    pub sys_exit: abi::Init,
    pub open: abi::Init,
    pub close: abi::Init,
    pub alloc: abi::Alloc,
    pub free: abi::Free,
    pub mmap: abi::Mmap,
    pub munmap: abi::Munmap,
    pub flush: abi::Flush,
    pub wait: abi::Wait,
    pub fill: abi::Fill,
    pub blit: abi::Blit,
}

// One process-wide session, with library loading independent of the active user count.
// Tests supply isolated drivers so their fake failure modes cannot affect one another.
#[derive(Default)]
struct Driver {
    api: OnceLock<Result<Api>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    users: usize,
    pending: bool,
}

pub(crate) struct Runtime {
    driver: Arc<Driver>,
    // Rc deliberately confines this runtime and all surface handles to their thread.
}

// This is the only vendor lock. Acquire it before entering any wrapper, and never
// acquire it recursively or hold it across a call to Runtime::lock/wait/open/drop.
// WaitAllDone waits only on already-submitted hardware work, never on another Rust
// caller or callback. Holding this lock during a blocking wait cannot form a lock
// cycle; other callers simply wait for DMA completion before entering the wrapper.
pub(crate) struct Calls<'a> {
    pub api: Api,
    state: MutexGuard<'a, State>,
}

impl Calls<'_> {
    pub fn mark_pending(&mut self) {
        self.state.pending = true;
    }

    pub fn wait(&mut self) -> Result<()> {
        if self.state.pending {
            // Wait for all operations, never compare wrapping fence numbers.
            check("MI_GFX_WaitAllDone", unsafe { (self.api.wait)(1, 0) })?;
            self.state.pending = false;
        }
        Ok(())
    }
}

pub(crate) fn check(operation: &'static str, code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Vendor { operation, code })
    }
}

impl Runtime {
    fn open(driver: Arc<Driver>, load: impl FnOnce() -> Result<Api>) -> Result<Rc<Self>> {
        {
            let mut state = driver.state.lock().unwrap_or_else(|e| e.into_inner());
            // Loading (including failures) happens once, under the vendor lock.
            let api = driver
                .api
                .get_or_init(load)
                .as_ref()
                .map_err(Clone::clone)?;
            if state.users == 0 {
                // SAFETY: the table has the exact vendored signatures and is resident.
                unsafe {
                    check("MI_SYS_Init", (api.sys_init)())?;
                    if let Err(error) = check("MI_GFX_Open", (api.open)()) {
                        (api.sys_exit)();
                        return Err(error);
                    }
                }
            }
            state.users += 1;
        }
        Ok(Rc::new(Self { driver }))
    }

    pub fn same_driver(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.driver, &other.driver)
    }

    pub fn lock(&self) -> Calls<'_> {
        let state = self.driver.state.lock().unwrap_or_else(|e| e.into_inner());
        Calls {
            api: *self.driver.api.get().unwrap().as_ref().unwrap(),
            state,
        }
    }

    pub fn wait(&self) -> Result<()> {
        self.lock().wait()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let mut calls = self.lock();
        if calls.state.users == 1 {
            if calls.wait().is_err() {
                // Unconfirmed DMA pins the initialized session as well as its libraries.
                return;
            }
            unsafe {
                (calls.api.close)();
                (calls.api.sys_exit)();
            }
        }
        calls.state.users -= 1;
    }
}

#[cfg(all(feature = "miyoo", target_os = "linux", target_arch = "arm"))]
mod loader {
    use super::*;
    use std::ffi::{CStr, CString};

    // Intentionally no Drop: vendor wrappers cache device descriptors without
    // closing them at unload. Pin even a partially loaded pair or missing-symbol
    // failure, since a library constructor may already have touched vendor state.
    struct Library(*mut libc::c_void);

    fn library(name: &str) -> Result<Library> {
        let mut errors = Vec::new();
        for path in [
            format!("/config/lib/{name}"),
            format!("/customer/lib/{name}"),
            name.to_owned(),
        ] {
            let path_c = CString::new(path.as_str()).unwrap();
            let handle =
                unsafe { libc::dlopen(path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                return Ok(Library(handle));
            }
            let error = unsafe { libc::dlerror() };
            let detail = if error.is_null() {
                "unknown loader error".into()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            errors.push(format!("{path}: {detail}"));
        }
        Err(Error::Unavailable(errors.join("; ")))
    }

    fn symbol(library: &Library, name: &CStr) -> Result<*mut libc::c_void> {
        unsafe {
            libc::dlerror();
            let address = libc::dlsym(library.0, name.as_ptr());
            let error = libc::dlerror();
            if !error.is_null() || address.is_null() {
                return Err(Error::Unavailable(format!(
                    "missing {}",
                    name.to_string_lossy()
                )));
            }
            Ok(address)
        }
    }

    pub(super) fn load_api() -> Result<Api> {
        let sys = library("libmi_sys.so")?;
        let gfx = library("libmi_gfx.so")?;
        macro_rules! get {
            ($lib:expr, $name:literal, $ty:ty) => {
                // SAFETY: each alias was checked against the local SDK header.
                unsafe { std::mem::transmute::<*mut libc::c_void, $ty>(symbol($lib, $name)?) }
            };
        }
        Ok(Api {
            sys_init: get!(&sys, c"MI_SYS_Init", abi::Init),
            sys_exit: get!(&sys, c"MI_SYS_Exit", abi::Init),
            open: get!(&gfx, c"MI_GFX_Open", abi::Init),
            close: get!(&gfx, c"MI_GFX_Close", abi::Init),
            alloc: get!(&sys, c"MI_SYS_MMA_Alloc", abi::Alloc),
            free: get!(&sys, c"MI_SYS_MMA_Free", abi::Free),
            mmap: get!(&sys, c"MI_SYS_Mmap", abi::Mmap),
            munmap: get!(&sys, c"MI_SYS_Munmap", abi::Munmap),
            flush: get!(&sys, c"MI_SYS_FlushInvCache", abi::Flush),
            wait: get!(&gfx, c"MI_GFX_WaitAllDone", abi::Wait),
            fill: get!(&gfx, c"MI_GFX_QuickFill", abi::Fill),
            blit: get!(&gfx, c"MI_GFX_BitBlit", abi::Blit),
        })
    }
}

pub(crate) fn load() -> Result<Rc<Runtime>> {
    #[cfg(all(feature = "miyoo", target_os = "linux", target_arch = "arm"))]
    {
        static DRIVER: OnceLock<Arc<Driver>> = OnceLock::new();
        Runtime::open(
            DRIVER.get_or_init(|| Arc::new(Driver::default())).clone(),
            loader::load_api,
        )
        .map_err(|e| Error::Unavailable(e.to_string()))
    }
    #[cfg(not(all(feature = "miyoo", target_os = "linux", target_arch = "arm")))]
    {
        Err(Error::Unavailable(
            "MI_GFX requires Linux ARM with feature miyoo".into(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        ffi::c_void,
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    #[derive(Default)]
    struct Overlap {
        active: AtomicUsize,
        peak: AtomicUsize,
        events: Mutex<Vec<&'static str>>,
    }

    thread_local! {
        static OBSERVER: RefCell<Option<Arc<Overlap>>> = const { RefCell::new(None) };
    }
    #[derive(Default)]
    struct State {
        events: Vec<&'static str>,
        fail: Option<&'static str>,
    }
    thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
    fn event(name: &'static str) -> i32 {
        OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow().as_ref() {
                let active = observer.active.fetch_add(1, Ordering::SeqCst) + 1;
                observer.peak.fetch_max(active, Ordering::SeqCst);
                observer.events.lock().unwrap().push(name);
                // Give the other caller time to enter if any wrapper lacks the lock.
                // In particular, fake WaitAllDone blocks without needing another call.
                thread::sleep(Duration::from_millis(1));
                observer.active.fetch_sub(1, Ordering::SeqCst);
            }
        });
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.events.push(name);
            if s.fail == Some(name) { -1 } else { 0 }
        })
    }
    pub fn reset(fail: Option<&'static str>) {
        STATE.with(|s| {
            *s.borrow_mut() = State {
                fail,
                ..State::default()
            }
        });
    }
    pub fn events() -> Vec<&'static str> {
        STATE.with(|s| s.borrow().events.clone())
    }
    unsafe extern "C" fn init() -> i32 {
        event("init")
    }
    unsafe extern "C" fn exit() -> i32 {
        event("exit")
    }
    unsafe extern "C" fn open() -> i32 {
        event("open")
    }
    unsafe extern "C" fn close() -> i32 {
        event("close")
    }
    unsafe extern "C" fn alloc(heap: *mut u8, len: u32, physical: *mut u64) -> i32 {
        assert!(heap.is_null());
        let code = event("alloc");
        if code == 0 {
            let mut ptr = std::ptr::null_mut();
            assert_eq!(
                unsafe {
                    libc::posix_memalign(
                        &mut ptr,
                        libc::sysconf(libc::_SC_PAGESIZE) as usize,
                        len as usize,
                    )
                },
                0
            );
            unsafe {
                std::ptr::write_bytes(ptr.cast::<u8>(), 0, len as usize);
                *physical = ptr as u64;
            }
        }
        code
    }
    unsafe extern "C" fn free(physical: u64) -> i32 {
        event("free");
        unsafe {
            libc::free(physical as *mut c_void);
        }
        0
    }
    unsafe extern "C" fn mmap(physical: u64, _: u32, out: *mut *mut c_void, cached: u8) -> i32 {
        assert_eq!(cached, 1);
        let code = event("mmap");
        if code == 0 {
            unsafe {
                *out = physical as *mut c_void;
            }
        }
        code
    }
    unsafe extern "C" fn munmap(_: *mut c_void, _: u32) -> i32 {
        event("munmap")
    }
    unsafe extern "C" fn flush(_: *mut c_void, _: u32) -> i32 {
        event("flush")
    }
    unsafe extern "C" fn wait(all: u8, fence: u16) -> i32 {
        assert_eq!((all, fence), (1, 0));
        event("wait")
    }
    unsafe extern "C" fn fill(
        _: *mut abi::Surface,
        _: *mut abi::Rect,
        _: u32,
        fence: *mut u16,
    ) -> i32 {
        unsafe {
            *fence = 65535;
        }
        event("fill")
    }
    unsafe extern "C" fn blit(
        _: *mut abi::Surface,
        _: *mut abi::Rect,
        _: *mut abi::Surface,
        _: *mut abi::Rect,
        _: *mut abi::Opt,
        fence: *mut u16,
    ) -> i32 {
        unsafe {
            *fence = 0;
        }
        event("blit")
    }
    fn api() -> Api {
        Api {
            sys_init: init,
            sys_exit: exit,
            open,
            close,
            alloc,
            free,
            mmap,
            munmap,
            flush,
            wait,
            fill,
            blit,
        }
    }
    pub fn runtime() -> Result<Rc<Runtime>> {
        Runtime::open(Arc::new(Driver::default()), || Ok(api()))
    }
    #[test]
    fn gfx_init_and_open_failure_are_balanced() {
        reset(Some("init"));
        assert!(runtime().is_err());
        assert_eq!(events(), ["init"]);
        reset(Some("open"));
        assert!(runtime().is_err());
        assert_eq!(events(), ["init", "open", "exit"]);
        reset(None);
        drop(runtime().unwrap());
        assert_eq!(events(), ["init", "open", "close", "exit"]);
    }

    fn fake_gfx(driver: &Arc<Driver>, loads: &AtomicUsize) -> super::super::Gfx {
        super::super::Gfx {
            runtime: Runtime::open(driver.clone(), || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(api())
            })
            .unwrap(),
        }
    }

    #[test]
    fn gfx_repeated_sessions_load_libraries_once() {
        reset(None);
        let driver = Arc::new(Driver::default());
        let loads = AtomicUsize::new(0);
        for _ in 0..32 {
            let gfx = fake_gfx(&driver, &loads);
            let surface = gfx.allocate(8, 8, abi::ARGB8888).unwrap();
            // A surface keeps initialization alive after the last Gfx handle drops.
            drop(gfx);
            assert_eq!(driver.state.lock().unwrap().users, 1);
            drop(surface);
            assert_eq!(driver.state.lock().unwrap().users, 0);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(
            events(),
            [
                "init", "open", "alloc", "mmap", "munmap", "free", "close", "exit"
            ]
            .repeat(32)
        );
    }

    #[test]
    fn gfx_reopen_after_init_failure_keeps_libraries_resident() {
        let driver = Arc::new(Driver::default());
        let loads = AtomicUsize::new(0);
        reset(Some("open"));
        assert!(
            Runtime::open(driver.clone(), || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(api())
            })
            .is_err()
        );
        assert_eq!(events(), ["init", "open", "exit"]);
        reset(None);
        drop(fake_gfx(&driver, &loads));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(events(), ["init", "open", "close", "exit"]);
    }

    #[test]
    fn gfx_concurrent_open_and_wrappers_are_serialized() {
        let driver = Arc::new(Driver::default());
        let loads = Arc::new(AtomicUsize::new(0));
        let observer = Arc::new(Overlap::default());
        let barrier = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            for _ in 0..2 {
                let driver = driver.clone();
                let loads = loads.clone();
                let observer = observer.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    reset(None);
                    OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer));
                    barrier.wait();
                    let gfx = fake_gfx(&driver, &loads);
                    // Keep both sessions alive before either can finish.
                    barrier.wait();
                    assert_eq!(driver.state.lock().unwrap().users, 2);
                    let src = gfx.allocate(8, 8, abi::ABGR8888).unwrap();
                    let mut dst = gfx.allocate(8, 8, abi::ARGB8888).unwrap();
                    for _ in 0..8 {
                        barrier.wait();
                        let rect = dst.rect();
                        gfx.prepare_fill(&mut dst, rect, 0)
                            .unwrap()
                            .submit()
                            .unwrap()
                            .wait()
                            .unwrap();
                        let rect = dst.rect();
                        gfx.prepare_blit(&src, src.rect(), &mut dst, rect, abi::Opt::copy(8, 8))
                            .unwrap()
                            .submit()
                            .unwrap()
                            .wait()
                            .unwrap();
                        dst.bytes_mut().unwrap();
                    }
                    drop((src, dst));
                    barrier.wait();
                    drop(gfx);
                    OBSERVER.with(|slot| *slot.borrow_mut() = None);
                });
            }
        });
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(observer.peak.load(Ordering::SeqCst), 1);
        let events = observer.events.lock().unwrap();
        assert_eq!(&events[..2], ["init", "open"]);
        assert_eq!(&events[events.len() - 2..], ["close", "exit"]);
        for name in ["init", "open", "close", "exit"] {
            assert_eq!(events.iter().filter(|&&e| e == name).count(), 1, "{name}");
        }
        for name in [
            "alloc", "free", "mmap", "munmap", "flush", "fill", "blit", "wait",
        ] {
            assert!(events.contains(&name), "missing wrapper coverage: {name}");
        }
        assert_eq!(driver.state.lock().unwrap().users, 0);
    }

    #[test]
    fn gfx_separate_handles_accept_shared_driver_surfaces() {
        reset(None);
        let driver = Arc::new(Driver::default());
        let loads = AtomicUsize::new(0);
        let a = fake_gfx(&driver, &loads);
        let b = fake_gfx(&driver, &loads);
        let mut surface = a.allocate(8, 8, abi::ARGB8888).unwrap();
        let rect = surface.rect();
        b.prepare_fill(&mut surface, rect, 0)
            .unwrap()
            .submit()
            .unwrap()
            .wait()
            .unwrap();
        drop(a);
        drop(b);
        assert!(!events().contains(&"close"));
        drop(surface);
        assert!(events().ends_with(&["close", "exit"]));
    }
}
