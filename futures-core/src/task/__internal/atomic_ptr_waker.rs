use core::cell::UnsafeCell;
use core::fmt;
use core::ptr::{addr_of, null_mut};
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

unsafe fn dummy_clone_register(_: *const ()) -> RawWaker {
    panic!("You shall not clone, this is only for register() locks")
}

unsafe fn dummy_clone_take(_: *const ()) -> RawWaker {
    panic!("You shall not clone, this is only for take() locks")
}

unsafe fn dummy_fn(_: *const ()) {
    panic!("You shall not wake or drop");
}

// Uses two different `clone()` to avoid different tables optimized into one.
static REGISTERING_TABLE: RawWakerVTable =
    RawWakerVTable::new(dummy_clone_register, dummy_fn, dummy_fn, dummy_fn);
static TAKING_TABLE: RawWakerVTable =
    RawWakerVTable::new(dummy_clone_take, dummy_fn, dummy_fn, dummy_fn);

const fn lock_val(r: &RawWakerVTable) -> *mut RawWakerVTable {
    r as *const _ as _
}

#[derive(PartialEq)]
enum Key {
    REGISTERING,
    TAKING,
}

use Key::*;

fn to_mut(k: Key) -> *mut RawWakerVTable {
    match k {
        REGISTERING => addr_of!(REGISTERING_TABLE) as *mut _,
        TAKING => addr_of!(TAKING_TABLE) as *mut _,
    }
}

impl AtomicWaker {
    pub const fn new() -> Self {
        Self { data: UnsafeCell::new(core::ptr::null()), vtable: AtomicPtr::new(null_mut()) }
    }

    //    struct Registration<'a, 'b>(&'a Self);

    /// Begins to register a [`Waker`], [`Err`] returns indicate unable to acquire the lock,
    /// `Err(true)` means a racing taker observed, note that `Err(false)` doesn't mean that there
    /// is no taker before this registration, in other words `Err(false)` does NOT imply that some
    /// one will do the wake-up later.
    #[inline]
    fn begin_register(&self) -> Result<(), bool> {
        // `register()` locking:
        //
        // * lock with REGISTERING, lose to REGISTERING or TAKING.
        // * unlock with real vtable (only done by the winning register)

        let old = self.vtable.swap(to_mut(REGISTERING), AcqRel);

        if old == to_mut(REGISTERING) {
            //debug_assert!(false);
            // Lose race with another register, do nothing.
            // Also no wake up guarantee if this happens.
            //
            // And we don't whether a taker races with us.

            Err(false)
        } else if old == to_mut(TAKING) {
            // Lose race with a taker, do nothing about the `.vtable`, the winning taker will unlock
            // it eventually.

            Err(true)
        } else {
            if old != null_mut() {
                // old waker still there, need to drop it

                let data = unsafe { *(self.data.get()) };

                // Re-construct the `Waker` with the exact `RawWaker` fields.
                unsafe {
                    let _ = Waker::from_raw(RawWaker::new(data, &*old));
                }
            }

            Ok(())
        }
    }

    /// The final action for registration, returning `true` means we observe a racing taker, unlike
    /// `begin_register()`, we may miss a racing taker because the ABA behavior of CAS: REGISTERING
    /// -> TAKING -> REGISTERING, but this only happens if there is a racing `register()`, and we
    /// don't guarantee no missing wake-up for that, so callers can simply skip a self wake-up if
    /// this returns false
    #[inline]
    fn end_register(&self, waker: &Waker) -> bool {
        // Set the new waker.
        let waker = waker.clone();

        unsafe {
            *(self.data.get()) = waker.as_raw().data();
        }

        if let Err(v) = self.vtable.compare_exchange(
            to_mut(REGISTERING),
            lock_val(waker.as_raw().vtable()),
            AcqRel,
            Acquire,
        ) {
            // Could only happen because of a racing taker.
            debug_assert_eq!(v, to_mut(TAKING));

            self.vtable.swap(null_mut(), Release);
            true
        } else {
            core::mem::forget(waker); // the `self` now owns `waker`.
            false
        }
    }

    pub fn register(&self, waker: &Waker) {
        match self.begin_register() {
            Ok(_) => {
                // Lock acquired, do the rest work.
                if self.end_register(waker) {
                    // Always do a self wakeup
                    waker.wake_by_ref();
                }
            }
            Err(true) => {
                waker.wake_by_ref();
            }
            Err(_) => {
                waker.wake_by_ref();
            }
        }
    }

    pub fn take(&self) -> Option<Waker> {
        // `take()` locking:
        //
        // * lock with TAKING, lose to REGISTERING or TAKING.
        // * unlock with `null_mut()` (only done by the winning register)

        let old = self.vtable.swap(to_mut(TAKING), AcqRel);

        if old == to_mut(REGISTERING) || old == to_mut(TAKING) {
            // lose race, nothing to do.
            None
        } else {
            let res = if old == null_mut() {
                // Already taken.
                None
            } else {
                let data = unsafe { *(self.data.get()) };

                Some(unsafe { Waker::from_raw(RawWaker::new(data, &*old)) })
            };

            // Unlocks no matter whether there is a [`Waker`] to take or not.
            self.vtable.store(null_mut(), Release);

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
