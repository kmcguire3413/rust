// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![stable]

//! Threadsafe reference-counted boxes (the `Arc<T>` type).
//!
//! The `Arc<T>` type provides shared ownership of an immutable value. Destruction is
//! deterministic, and will occur as soon as the last owner is gone. It is marked as `Send` because
//! it uses atomic reference counting.
//!
//! If you do not need thread-safety, and just need shared ownership, consider the [`Rc<T>`
//! type](../rc/struct.Rc.html). It is the same as `Arc<T>`, but does not use atomics, making it
//! both thread-unsafe as well as significantly faster when updating the reference count.
//!
//! The `downgrade` method can be used to create a non-owning `Weak<T>` pointer to the box. A
//! `Weak<T>` pointer can be upgraded to an `Arc<T>` pointer, but will return `None` if the value
//! has already been dropped.
//!
//! For example, a tree with parent pointers can be represented by putting the nodes behind strong
//! `Arc<T>` pointers, and then storing the parent pointers as `Weak<T>` pointers.
//!
//! # Examples
//!
//! Sharing some immutable data between tasks:
//!
//! ```
//! use std::sync::Arc;
//! use std::thread::Thread;
//!
//! let five = Arc::new(5i);
//!
//! for i in range(0u, 10) {
//!     let five = five.clone();
//!
//!     Thread::spawn(move || {
//!         println!("{}", five);
//!     }).detach();
//! }
//! ```
//!
//! Sharing mutable data safely between tasks with a `Mutex`:
//!
//! ```
//! use std::sync::{Arc, Mutex};
//! use std::thread::Thread;
//!
//! let five = Arc::new(Mutex::new(5i));
//!
//! for _ in range(0u, 10) {
//!     let five = five.clone();
//!
//!     Thread::spawn(move || {
//!         let mut number = five.lock().unwrap();
//!
//!         *number += 1;
//!
//!         println!("{}", *number); // prints 6
//!     }).detach();
//! }
//! ```

use core::atomic;
use core::atomic::Ordering::{Relaxed, Release, Acquire, SeqCst};
use core::borrow::BorrowFrom;
use core::clone::Clone;
use core::fmt::{mod, Show};
use core::cmp::{Eq, Ord, PartialEq, PartialOrd, Ordering};
use core::default::Default;
use core::kinds::{Sync, Send};
use core::mem::{min_align_of, size_of, drop};
use core::mem;
use core::nonzero::NonZero;
use core::ops::{Drop, Deref};
use core::option::Option;
use core::option::Option::{Some, None};
use core::ptr::{mod, PtrExt};
use heap::deallocate;

/// An atomically reference counted wrapper for shared state.
///
/// # Example
///
/// In this example, a large vector of floats is shared between several tasks. With simple pipes,
/// without `Arc`, a copy would have to be made for each task.
///
/// ```rust
/// use std::sync::Arc;
/// use std::thread::Thread;
///
/// fn main() {
///     let numbers: Vec<_> = range(0, 100u32).map(|i| i as f32).collect();
///     let shared_numbers = Arc::new(numbers);
///
///     for _ in range(0u, 10) {
///         let child_numbers = shared_numbers.clone();
///
///         Thread::spawn(move || {
///             let local_numbers = child_numbers.as_slice();
///
///             // Work with the local numbers
///         }).detach();
///     }
/// }
/// ```
#[unsafe_no_drop_flag]
#[stable]
pub struct Arc<T> {
    // FIXME #12808: strange name to try to avoid interfering with
    // field accesses of the contained type via Deref
    _ptr: NonZero<*mut ArcInner<T>>,
}

unsafe impl<T: Sync + Send> Send for Arc<T> { }
unsafe impl<T: Sync + Send> Sync for Arc<T> { }


/// A weak pointer to an `Arc`.
///
/// Weak pointers will not keep the data inside of the `Arc` alive, and can be used to break cycles
/// between `Arc` pointers.
#[unsafe_no_drop_flag]
#[experimental = "Weak pointers may not belong in this module."]
pub struct Weak<T> {
    // FIXME #12808: strange name to try to avoid interfering with
    // field accesses of the contained type via Deref
    _ptr: NonZero<*mut ArcInner<T>>,
}

unsafe impl<T: Sync + Send> Send for Weak<T> { }
unsafe impl<T: Sync + Send> Sync for Weak<T> { }

struct ArcInner<T> {
    strong: atomic::AtomicUint,
    weak: atomic::AtomicUint,
    data: T,
}

unsafe impl<T: Sync + Send> Send for ArcInner<T> {}
unsafe impl<T: Sync + Send> Sync for ArcInner<T> {}

impl<T> Arc<T> {
    /// Constructs a new `Arc<T>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    /// ```
    #[inline]
    #[stable]
    pub fn new(data: T) -> Arc<T> {
        // Start the weak pointer count as 1 which is the weak pointer that's
        // held by all the strong pointers (kinda), see std/rc.rs for more info
        let x = box ArcInner {
            strong: atomic::AtomicUint::new(1),
            weak: atomic::AtomicUint::new(1),
            data: data,
        };
        Arc { _ptr: unsafe { NonZero::new(mem::transmute(x)) } }
    }

    /// Downgrades the `Arc<T>` to a `Weak<T>` reference.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// let weak_five = five.downgrade();
    /// ```
    #[experimental = "Weak pointers may not belong in this module."]
    pub fn downgrade(&self) -> Weak<T> {
        // See the clone() impl for why this is relaxed
        self.inner().weak.fetch_add(1, Relaxed);
        Weak { _ptr: self._ptr }
    }
}

impl<T> Arc<T> {
    #[inline]
    fn inner(&self) -> &ArcInner<T> {
        // This unsafety is ok because while this arc is alive we're guaranteed that the inner
        // pointer is valid. Furthermore, we know that the `ArcInner` structure itself is `Sync`
        // because the inner data is `Sync` as well, so we're ok loaning out an immutable pointer
        // to these contents.
        unsafe { &**self._ptr }
    }
}

/// Get the number of weak references to this value.
#[inline]
#[experimental]
pub fn weak_count<T>(this: &Arc<T>) -> uint { this.inner().weak.load(SeqCst) - 1 }

/// Get the number of strong references to this value.
#[inline]
#[experimental]
pub fn strong_count<T>(this: &Arc<T>) -> uint { this.inner().strong.load(SeqCst) }

#[stable]
impl<T> Clone for Arc<T> {
    /// Makes a clone of the `Arc<T>`.
    ///
    /// This increases the strong reference count.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five.clone();
    /// ```
    #[inline]
    fn clone(&self) -> Arc<T> {
        // Using a relaxed ordering is alright here, as knowledge of the original reference
        // prevents other threads from erroneously deleting the object.
        //
        // As explained in the [Boost documentation][1], Increasing the reference counter can
        // always be done with memory_order_relaxed: New references to an object can only be formed
        // from an existing reference, and passing an existing reference from one thread to another
        // must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        self.inner().strong.fetch_add(1, Relaxed);
        Arc { _ptr: self._ptr }
    }
}

impl<T> BorrowFrom<Arc<T>> for T {
    fn borrow_from(owned: &Arc<T>) -> &T {
        &**owned
    }
}

#[experimental = "Deref is experimental."]
impl<T> Deref<T> for Arc<T> {
    #[inline]
    fn deref(&self) -> &T {
        &self.inner().data
    }
}

impl<T: Send + Sync + Clone> Arc<T> {
    /// Make a mutable reference from the given `Arc<T>`.
    ///
    /// This is also referred to as a copy-on-write operation because the inner data is cloned if
    /// the reference count is greater than one.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let mut five = Arc::new(5i);
    ///
    /// let mut_five = five.make_unique();
    /// ```
    #[inline]
    #[experimental]
    pub fn make_unique(&mut self) -> &mut T {
        // Note that we hold a strong reference, which also counts as a weak reference, so we only
        // clone if there is an additional reference of either kind.
        if self.inner().strong.load(SeqCst) != 1 ||
           self.inner().weak.load(SeqCst) != 1 {
            *self = Arc::new((**self).clone())
        }
        // This unsafety is ok because we're guaranteed that the pointer returned is the *only*
        // pointer that will ever be returned to T. Our reference count is guaranteed to be 1 at
        // this point, and we required the Arc itself to be `mut`, so we're returning the only
        // possible reference to the inner data.
        let inner = unsafe { &mut **self._ptr };
        &mut inner.data
    }
}

#[unsafe_destructor]
#[experimental = "waiting on stability of Drop"]
impl<T: Sync + Send> Drop for Arc<T> {
    /// Drops the `Arc<T>`.
    ///
    /// This will decrement the strong reference count. If the strong reference count becomes zero
    /// and the only other references are `Weak<T>` ones, `drop`s the inner value.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// {
    ///     let five = Arc::new(5i);
    ///
    ///     // stuff
    ///
    ///     drop(five); // explict drop
    /// }
    /// {
    ///     let five = Arc::new(5i);
    ///
    ///     // stuff
    ///
    /// } // implicit drop
    /// ```
    fn drop(&mut self) {
        // This structure has #[unsafe_no_drop_flag], so this drop glue may run more than once (but
        // it is guaranteed to be zeroed after the first if it's run more than once)
        let ptr = *self._ptr;
        if ptr.is_null() { return }

        // Because `fetch_sub` is already atomic, we do not need to synchronize with other threads
        // unless we are going to delete the object. This same logic applies to the below
        // `fetch_sub` to the `weak` count.
        if self.inner().strong.fetch_sub(1, Release) != 1 { return }

        // This fence is needed to prevent reordering of use of the data and deletion of the data.
        // Because it is marked `Release`, the decreasing of the reference count synchronizes with
        // this `Acquire` fence. This means that use of the data happens before decreasing the
        // reference count, which happens before this fence, which happens before the deletion of
        // the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one thread (through an
        // > existing reference) to *happen before* deleting the object in a different thread. This
        // > is achieved by a "release" operation after dropping a reference (any access to the
        // > object through this reference must obviously happened before), and an "acquire"
        // > operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        atomic::fence(Acquire);

        // Destroy the data at this time, even though we may not free the box allocation itself
        // (there may still be weak pointers lying around).
        unsafe { drop(ptr::read(&self.inner().data)); }

        if self.inner().weak.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);
            unsafe { deallocate(ptr as *mut u8, size_of::<ArcInner<T>>(),
                                min_align_of::<ArcInner<T>>()) }
        }
    }
}

#[experimental = "Weak pointers may not belong in this module."]
impl<T: Sync + Send> Weak<T> {
    /// Upgrades a weak reference to a strong reference.
    ///
    /// Upgrades the `Weak<T>` reference to an `Arc<T>`, if possible.
    ///
    /// Returns `None` if there were no strong references and the data was destroyed.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// let weak_five = five.downgrade();
    ///
    /// let strong_five: Option<Arc<_>> = weak_five.upgrade();
    /// ```
    pub fn upgrade(&self) -> Option<Arc<T>> {
        // We use a CAS loop to increment the strong count instead of a fetch_add because once the
        // count hits 0 is must never be above 0.
        let inner = self.inner();
        loop {
            let n = inner.strong.load(SeqCst);
            if n == 0 { return None }
            let old = inner.strong.compare_and_swap(n, n + 1, SeqCst);
            if old == n { return Some(Arc { _ptr: self._ptr }) }
        }
    }

    #[inline]
    fn inner(&self) -> &ArcInner<T> {
        // See comments above for why this is "safe"
        unsafe { &**self._ptr }
    }
}

#[experimental = "Weak pointers may not belong in this module."]
impl<T: Sync + Send> Clone for Weak<T> {
    /// Makes a clone of the `Weak<T>`.
    ///
    /// This increases the weak reference count.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let weak_five = Arc::new(5i).downgrade();
    ///
    /// weak_five.clone();
    /// ```
    #[inline]
    fn clone(&self) -> Weak<T> {
        // See comments in Arc::clone() for why this is relaxed
        self.inner().weak.fetch_add(1, Relaxed);
        Weak { _ptr: self._ptr }
    }
}

#[unsafe_destructor]
#[experimental = "Weak pointers may not belong in this module."]
impl<T: Sync + Send> Drop for Weak<T> {
    /// Drops the `Weak<T>`.
    ///
    /// This will decrement the weak reference count.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// {
    ///     let five = Arc::new(5i);
    ///     let weak_five = five.downgrade();
    ///
    ///     // stuff
    ///
    ///     drop(weak_five); // explict drop
    /// }
    /// {
    ///     let five = Arc::new(5i);
    ///     let weak_five = five.downgrade();
    ///
    ///     // stuff
    ///
    /// } // implicit drop
    /// ```
    fn drop(&mut self) {
        let ptr = *self._ptr;

        // see comments above for why this check is here
        if ptr.is_null() { return }

        // If we find out that we were the last weak pointer, then its time to deallocate the data
        // entirely. See the discussion in Arc::drop() about the memory orderings
        if self.inner().weak.fetch_sub(1, Release) == 1 {
            atomic::fence(Acquire);
            unsafe { deallocate(ptr as *mut u8, size_of::<ArcInner<T>>(),
                                min_align_of::<ArcInner<T>>()) }
        }
    }
}

#[stable]
impl<T: PartialEq> PartialEq for Arc<T> {
    /// Equality for two `Arc<T>`s.
    ///
    /// Two `Arc<T>`s are equal if their inner value are equal.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five == Arc::new(5i);
    /// ```
    fn eq(&self, other: &Arc<T>) -> bool { *(*self) == *(*other) }

    /// Inequality for two `Arc<T>`s.
    ///
    /// Two `Arc<T>`s are unequal if their inner value are unequal.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five != Arc::new(5i);
    /// ```
    fn ne(&self, other: &Arc<T>) -> bool { *(*self) != *(*other) }
}
#[stable]
impl<T: PartialOrd> PartialOrd for Arc<T> {
    /// Partial comparison for two `Arc<T>`s.
    ///
    /// The two are compared by calling `partial_cmp()` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five.partial_cmp(&Arc::new(5i));
    /// ```
    fn partial_cmp(&self, other: &Arc<T>) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }

    /// Less-than comparison for two `Arc<T>`s.
    ///
    /// The two are compared by calling `<` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five < Arc::new(5i);
    /// ```
    fn lt(&self, other: &Arc<T>) -> bool { *(*self) < *(*other) }

    /// 'Less-than or equal to' comparison for two `Arc<T>`s.
    ///
    /// The two are compared by calling `<=` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five <= Arc::new(5i);
    /// ```
    fn le(&self, other: &Arc<T>) -> bool { *(*self) <= *(*other) }

    /// Greater-than comparison for two `Arc<T>`s.
    ///
    /// The two are compared by calling `>` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five > Arc::new(5i);
    /// ```
    fn gt(&self, other: &Arc<T>) -> bool { *(*self) > *(*other) }

    /// 'Greater-than or equal to' comparison for two `Arc<T>`s.
    ///
    /// The two are compared by calling `>=` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5i);
    ///
    /// five >= Arc::new(5i);
    /// ```
    fn ge(&self, other: &Arc<T>) -> bool { *(*self) >= *(*other) }
}
#[stable]
impl<T: Ord> Ord for Arc<T> {
    fn cmp(&self, other: &Arc<T>) -> Ordering { (**self).cmp(&**other) }
}
#[stable]
impl<T: Eq> Eq for Arc<T> {}

impl<T: fmt::Show> fmt::Show for Arc<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[stable]
impl<T: Default + Sync + Send> Default for Arc<T> {
    #[stable]
    fn default() -> Arc<T> { Arc::new(Default::default()) }
}

#[cfg(test)]
#[allow(experimental)]
mod tests {
    use std::clone::Clone;
    use std::comm::channel;
    use std::mem::drop;
    use std::ops::Drop;
    use std::option::Option;
    use std::option::Option::{Some, None};
    use std::str::Str;
    use std::sync::atomic;
    use std::sync::atomic::Ordering::{Acquire, SeqCst};
    use std::task;
    use std::kinds::Send;
    use std::vec::Vec;
    use super::{Arc, Weak, weak_count, strong_count};
    use std::sync::Mutex;

    struct Canary(*mut atomic::AtomicUint);

    impl Drop for Canary
    {
        fn drop(&mut self) {
            unsafe {
                match *self {
                    Canary(c) => {
                        (*c).fetch_add(1, SeqCst);
                    }
                }
            }
        }
    }

    #[test]
    fn manually_share_arc() {
        let v = vec!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
        let arc_v = Arc::new(v);

        let (tx, rx) = channel();

        task::spawn(move || {
            let arc_v: Arc<Vec<int>> = rx.recv();
            assert_eq!((*arc_v)[3], 4);
        });

        tx.send(arc_v.clone());

        assert_eq!((*arc_v)[2], 3);
        assert_eq!((*arc_v)[4], 5);
    }

    #[test]
    fn test_cowarc_clone_make_unique() {
        let mut cow0 = Arc::new(75u);
        let mut cow1 = cow0.clone();
        let mut cow2 = cow1.clone();

        assert!(75 == *cow0.make_unique());
        assert!(75 == *cow1.make_unique());
        assert!(75 == *cow2.make_unique());

        *cow0.make_unique() += 1;
        *cow1.make_unique() += 2;
        *cow2.make_unique() += 3;

        assert!(76 == *cow0);
        assert!(77 == *cow1);
        assert!(78 == *cow2);

        // none should point to the same backing memory
        assert!(*cow0 != *cow1);
        assert!(*cow0 != *cow2);
        assert!(*cow1 != *cow2);
    }

    #[test]
    fn test_cowarc_clone_unique2() {
        let mut cow0 = Arc::new(75u);
        let cow1 = cow0.clone();
        let cow2 = cow1.clone();

        assert!(75 == *cow0);
        assert!(75 == *cow1);
        assert!(75 == *cow2);

        *cow0.make_unique() += 1;

        assert!(76 == *cow0);
        assert!(75 == *cow1);
        assert!(75 == *cow2);

        // cow1 and cow2 should share the same contents
        // cow0 should have a unique reference
        assert!(*cow0 != *cow1);
        assert!(*cow0 != *cow2);
        assert!(*cow1 == *cow2);
    }

    #[test]
    fn test_cowarc_clone_weak() {
        let mut cow0 = Arc::new(75u);
        let cow1_weak = cow0.downgrade();

        assert!(75 == *cow0);
        assert!(75 == *cow1_weak.upgrade().unwrap());

        *cow0.make_unique() += 1;

        assert!(76 == *cow0);
        assert!(cow1_weak.upgrade().is_none());
    }

    #[test]
    fn test_live() {
        let x = Arc::new(5i);
        let y = x.downgrade();
        assert!(y.upgrade().is_some());
    }

    #[test]
    fn test_dead() {
        let x = Arc::new(5i);
        let y = x.downgrade();
        drop(x);
        assert!(y.upgrade().is_none());
    }

    #[test]
    fn weak_self_cyclic() {
        struct Cycle {
            x: Mutex<Option<Weak<Cycle>>>
        }

        let a = Arc::new(Cycle { x: Mutex::new(None) });
        let b = a.clone().downgrade();
        *a.x.lock().unwrap() = Some(b);

        // hopefully we don't double-free (or leak)...
    }

    #[test]
    fn drop_arc() {
        let mut canary = atomic::AtomicUint::new(0);
        let x = Arc::new(Canary(&mut canary as *mut atomic::AtomicUint));
        drop(x);
        assert!(canary.load(Acquire) == 1);
    }

    #[test]
    fn drop_arc_weak() {
        let mut canary = atomic::AtomicUint::new(0);
        let arc = Arc::new(Canary(&mut canary as *mut atomic::AtomicUint));
        let arc_weak = arc.downgrade();
        assert!(canary.load(Acquire) == 0);
        drop(arc);
        assert!(canary.load(Acquire) == 1);
        drop(arc_weak);
    }

    #[test]
    fn test_strong_count() {
        let a = Arc::new(0u32);
        assert!(strong_count(&a) == 1);
        let w = a.downgrade();
        assert!(strong_count(&a) == 1);
        let b = w.upgrade().expect("");
        assert!(strong_count(&b) == 2);
        assert!(strong_count(&a) == 2);
        drop(w);
        drop(a);
        assert!(strong_count(&b) == 1);
        let c = b.clone();
        assert!(strong_count(&b) == 2);
        assert!(strong_count(&c) == 2);
    }

    #[test]
    fn test_weak_count() {
        let a = Arc::new(0u32);
        assert!(strong_count(&a) == 1);
        assert!(weak_count(&a) == 0);
        let w = a.downgrade();
        assert!(strong_count(&a) == 1);
        assert!(weak_count(&a) == 1);
        let x = w.clone();
        assert!(weak_count(&a) == 2);
        drop(w);
        drop(x);
        assert!(strong_count(&a) == 1);
        assert!(weak_count(&a) == 0);
        let c = a.clone();
        assert!(strong_count(&a) == 2);
        assert!(weak_count(&a) == 0);
        let d = c.downgrade();
        assert!(weak_count(&c) == 1);
        assert!(strong_count(&c) == 2);

        drop(a);
        drop(c);
        drop(d);
    }

    #[test]
    fn show_arc() {
        let a = Arc::new(5u32);
        assert!(format!("{}", a) == "5")
    }

    // Make sure deriving works with Arc<T>
    #[deriving(Eq, Ord, PartialEq, PartialOrd, Clone, Show, Default)]
    struct Foo { inner: Arc<int> }
}
