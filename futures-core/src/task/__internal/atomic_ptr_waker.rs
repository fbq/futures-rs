use core::cell::UnsafeCell;
use core::fmt;
use core::ptr::addr_of;
use core::task::{RawWaker, RawWakerVTable, Waker};

use atomic::AtomicPtr;
use atomic::Ordering::{AcqRel, Acquire, Release};

#[cfg(feature = "portable-atomic")]
use portable_atomic as atomic;

#[cfg(not(feature = "portable-atomic"))]
use core::sync::atomic;

pub struct AtomicWaker {
    data: UnsafeCell<*const ()>,
    vtable: AtomicPtr<RawWakerVTable>,
}

unsafe fn dummy_clone1(_: *const ()) -> RawWaker {
    panic!("You shall not clone1")
}

unsafe fn dummy_clone2(_: *const ()) -> RawWaker {
    panic!("You shall not clone2")
}

unsafe fn dummy_clone3(_: *const ()) -> RawWaker {
    panic!("You shall not clone3")
}

unsafe fn dummy_fn(_: *const ()) {
    panic!("You shall not wake or drop");
}

static TAKING_TABLE: RawWakerVTable =
    RawWakerVTable::new(dummy_clone1, dummy_fn, dummy_fn, dummy_fn);
static TAKEN_TABLE: RawWakerVTable =
    RawWakerVTable::new(dummy_clone2, dummy_fn, dummy_fn, dummy_fn);
static REGISTERING_TABLE: RawWakerVTable =
    RawWakerVTable::new(dummy_clone3, dummy_fn, dummy_fn, dummy_fn);

//const REGISTERING: *const RawWakerVTable = addr_of!(REGISTERING_TABLE);
//const TAKING: *const RawWakerVTable = addr_of!(TAKING_TABLE);
//const TAKEN: *const RawWakerVTable = addr_of!(TAKEN_TABLE);

const fn lock_val(r: &RawWakerVTable) -> *mut RawWakerVTable {
    r as *const _ as _
}

#[derive(PartialEq)]
enum Key {
    REGISTERING,
    TAKEN,
    TAKING,
}

use Key::*;

fn to_mut(k: Key) -> *mut RawWakerVTable {
    match k {
        REGISTERING => addr_of!(REGISTERING_TABLE) as *mut _,
        TAKING => addr_of!(TAKING_TABLE) as *mut _,
        TAKEN => addr_of!(TAKEN_TABLE) as *mut _,
    }
}

impl AtomicWaker {
    pub fn new() -> Self {
        Self { data: UnsafeCell::new(core::ptr::null()), vtable: AtomicPtr::new(to_mut(TAKEN)) }
    }

    pub fn register(&self, waker: &Waker) {
        // register locking rules:
        // * lock with REGISTERING
        // * unlock with real vtable (only done by the winning register)
        let old = self.vtable.swap(to_mut(REGISTERING), AcqRel);

        if old == to_mut(REGISTERING) {
            // Lose race with another register, do nothing.
            // Also no wake up guarantee if this happens.
        } else if old == to_mut(TAKING) {
            // Lose race with a taker, do nothing about the `.vtable`, the winning taker will unlock
            // it eventually.

            // Do a wake up here since it's unable to set the new waker and we don't want to miss
            // wake-ups.
            //
            // Potential optimization: if taking a waker means `Poll::Ready` for example, timers,
            // we may not need wake-ups.
            waker.wake_by_ref(); // re-poll
        } else {
            if old != to_mut(TAKEN) {
                // old waker still there, need to drop it

                let data = unsafe { *(self.data.get()) };

                // Re-construct the `Waker` with the exact `RawWaker` fields.
                unsafe {
                    let _ = Waker::from_raw(RawWaker::new(data, &*old));
                }
            }

            // Set the new waker.
            let waker = waker.clone();
            unsafe {
                *(self.data.get()) = waker.as_raw().data();
            }

            let _ = self
                .vtable
                .compare_exchange(
                    to_mut(REGISTERING),
                    lock_val(waker.as_raw().vtable()),
                    AcqRel,
                    Acquire,
                )
                .map_err(|v| {
                    // Could only happen because of a racing taker.
                    debug_assert_eq!(v, to_mut(TAKING));

                    // Race with a taker in the middle of setting the new waker
                    //
                    // Potential optimization: if taking a waker means `Poll::Ready` for example, timers,
                    // we may not need wake-ups.
                    waker.wake_by_ref();

                    self.vtable.store(lock_val(waker.as_raw().vtable()), Release);
                });

            core::mem::forget(waker); // the `self` now owns `waker`.
        }
    }

    pub fn take(&self) -> Option<Waker> {
        let old = self.vtable.swap(to_mut(TAKING), AcqRel);

        if old == to_mut(REGISTERING) || old == to_mut(TAKING) {
            // lose race, nothing to do.
            None
        } else {
            let res = if old == to_mut(TAKEN) {
                None
            } else {
                let data = unsafe { *(self.data.get()) };

                Some(unsafe { Waker::from_raw(RawWaker::new(data, &*old)) })
            };

            self.vtable.store(to_mut(TAKEN), Release);

            res
        }
    }

    pub fn wake(&self) {
        if let Some(waker) = self.take() {
            waker.wake();
        }
    }
}

impl Default for AtomicWaker {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AtomicWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicWaker")
    }
}

impl Drop for AtomicWaker {
    fn drop(&mut self) {
        self.take();
    }
}

unsafe impl Send for AtomicWaker {}
unsafe impl Sync for AtomicWaker {}
