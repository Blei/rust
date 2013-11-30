// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*!
 * Library to interface with chunks of memory allocated in C.
 *
 * It is often desirable to safely interface with memory allocated from C,
 * encapsulating the unsafety into allocation and destruction time.  Indeed,
 * allocating memory externally is currently the only way to give Rust shared
 * mut state with C programs that keep their own references; vectors are
 * unsuitable because they could be reallocated or moved at any time, and
 * importing C memory into a vector takes a one-time snapshot of the memory.
 *
 * This module simplifies the usage of such external blocks of memory.  Memory
 * is encapsulated into an opaque object after creation; the lifecycle of the
 * memory can be optionally managed by Rust, if an appropriate destructor
 * closure is provided.  Safety is ensured by bounds-checking accesses, which
 * are marshalled through get and set functions.
 */

use std::ptr;

/**
 * The type representing a foreign chunk of memory
 */
pub struct CVec<T> {
    priv base: *mut T,
    priv len: uint,
    priv rsrc: DtorRes,
}

struct DtorRes {
    dtor: Option<proc()>,
}

#[unsafe_destructor]
impl Drop for DtorRes {
    fn drop(&mut self) {
        let dtor = self.dtor.take();
        match dtor {
            None => (),
            Some(f) => f()
        }
    }
}

impl DtorRes {
    fn new(dtor: Option<proc()>) -> DtorRes {
        DtorRes {
            dtor: dtor,
        }
    }
}

/*
 Section: Introduction forms
 */

impl <T> CVec<T> {
    /**
     * Create a `CVec` from a foreign buffer with a given length.
     *
     * # Arguments
     *
     * * base - A foreign pointer to a buffer
     * * len - The number of elements in the buffer
     */
    pub fn new(base: *mut T, len: uint) -> CVec<T> {
        CVec {
            base: base,
            len: len,
            rsrc: DtorRes::new(None)
        }
    }

    /**
     * Create a `CVec` from a foreign buffer, with a given length,
     * and a function to run upon destruction.
     *
     * # Arguments
     *
     * * base - A foreign pointer to a buffer
     * * len - The number of elements in the buffer
     * * dtor - A function to run when the value is destructed, useful
     *          for freeing the buffer, etc.
     */
    pub fn new_with_dtor(base: *mut T, len: uint, dtor: proc()) -> CVec<T> {
        CVec {
            base: base,
            len: len,
            rsrc: DtorRes::new(Some(dtor))
        }
    }

    /**
     * Sets the value of an element at a given index
     *
     * Fails if `ofs` is greater or equal to the length of the vector
     */
    pub unsafe fn set(&mut self, ofs: uint, v: T) {
        assert!(ofs < self.len);
        *ptr::mut_offset(self.base, ofs as int) = v;
    }

    /// Returns the length of the vector
    pub fn len(&self) -> uint { self.len }

    /// Calls a closure with a reference to the underlying pointer
    pub fn with_ptr<U>(&self, f: |*mut T| -> U) -> U {
        f(self.base)
    }
}

impl <T: Clone> CVec<T> {
    /**
     * Retrieves an element at a given index
     *
     * Fails if `ofs` is greater or equal to the length of the vector
     */
    pub unsafe fn get(&self, ofs: uint) -> T {
        assert!(ofs < self.len);
        (*ptr::mut_offset(self.base, ofs as int)).clone()
    }
}

#[cfg(test)]
mod tests {

    use c_vec::*;

    use std::libc::*;
    use std::libc;

    fn malloc(n: uint) -> CVec<u8> {
        unsafe {
            let mem = libc::malloc(n as size_t);

            assert!(mem as int != 0);

            return CVec::new_with_dtor(mem as *mut u8, n,
                proc() unsafe { libc::free(mem); });
        }
    }

    #[test]
    fn test_basic() {
        let mut cv = malloc(16);

        unsafe {
            cv.set(3, 8u8);
            cv.set(4, 9u8);
            assert_eq!(cv.get(3u), 8u8);
            assert_eq!(cv.get(4u), 9u8);
        }
        assert_eq!(cv.len(), 16u);
    }

    #[test]
    #[should_fail]
    fn test_overrun_get() {
        let cv = malloc(16);

        unsafe { cv.get(17u) };
    }

    #[test]
    #[should_fail]
    fn test_overrun_set() {
        let mut cv = malloc(16);

        unsafe { cv.set(17u, 0u8) };
    }

    #[test]
    fn test_and_I_mean_it() {
        let mut cv = malloc(16);

        unsafe {
            cv.set(0u, 32u8);
            cv.set(1u, 33u8);
            cv.with_ptr(|p| {
                assert_eq!(*p, 32u8);
            });
            cv.set(2u, 34u8); /* safety */
        }
    }

}
