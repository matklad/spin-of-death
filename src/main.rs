use std::{os::unix::thread::JoinHandleExt, sync::atomic::AtomicUsize, thread};

use thread_priority::{
    set_thread_priority, thread_native_id, RealtimeThreadSchedulePolicy, ThreadPriority,
    ThreadSchedulePolicy,
};

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms))
}

static BARRIER: AtomicUsize = AtomicUsize::new(0);
const N_THREADS: usize = 512;

fn main() {
    std::sync::Arc::get_mut
    let pool: Pool<i32, _> = Pool::new(i32::default);
    std::thread::scope(|scope| {
        // This is a low-priority thread, which will enter a spin-lock protected critical section in `getrandom` crate.
        let t = scope.spawn(|| {
            set_priority(ThreadPriority::Min);
            let _guard = pool.get();
        });

        // These are high priority threads, which will try to grab a lock held by
        // the first thread.
        for _ in 0..N_THREADS {
            let t = scope.spawn(|| {
                set_priority(ThreadPriority::Max);
                sleep_ms(100); // Make sure that we don't accidently grab the lock first.
                BARRIER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                pool.get();
            });
        }
    });
}

fn set_priority(priority: ThreadPriority) {
    set_thread_priority(
        thread_native_id(),
        priority,
        ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::RoundRobin),
    )
    .unwrap();
}

extern crate alloc;

use alloc::boxed::Box;
use core::{
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{
        AtomicPtr,
        Ordering::{Acquire, Relaxed, Release},
    },
};

pub struct Pool<T, F = fn() -> T> {
    create: F,
    /// Pointer to the head of the linked list of free nodes.
    ///
    /// Null if there are no free nodes.
    ///
    /// LOCKED if the list is locked, which only happens briefly
    /// when removing a node, not when adding a node back or when
    /// allocating a new node.
    head: AtomicPtr<Node<T>>,
}

// Safety: Using the same Pool from multiple fines is fine as
// long as F can be called concurrently and ownership of objects of type T
// can be transferred between threads.
unsafe impl<T: Send, F: Sync> Sync for Pool<T, F> {}
// Safety: Moving a pool to another thread entirely is fine as long as both T
// and F allow that.
unsafe impl<T: Send, F: Send> Send for Pool<T, F> {}

/// Special value we use for the `head` pointer to incicate that the pool is locked.
const LOCKED: *mut Node<()> = usize::MAX as *mut _;

struct Node<T> {
    next: AtomicPtr<Node<T>>,
    value: T,
}

impl<T, F> Pool<T, F> {
    pub fn new(create: F) -> Pool<T, F> {
        Pool {
            create,
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

pub struct PoolGuard<'a, T, F> {
    pool: &'a Pool<T, F>,
    node: *mut Node<T>,
}

// Safety: Sharing a PoolGuard with another thread effectively
// shares the T with the other thread.
// So the PoolGuard is Sync if T is Sync.
unsafe impl<T: Sync, F> Sync for PoolGuard<'_, T, F> {}
// Safety: Moving a PoolGuard to another thread effectively
// moves the exclusive access to the T to the other thread.
// So the PoolGuard is Send if T is Send.
unsafe impl<T: Send, F> Send for PoolGuard<'_, T, F> {}

impl<T, F: Fn() -> T> Pool<T, F> {
    pub fn get(&self) -> PoolGuard<'_, T, F> {
        let mut node = self.head.load(Relaxed);
        while !node.is_null() {
            if node == LOCKED.cast() {
                // Locked! Try again!
                core::hint::spin_loop();
                node = self.head.load(Relaxed);
                continue;
            }
            // Take the head node and lock the list.
            // We need to briefly lock the list, so we have time to check the
            // `next` pointer of the head node without it changing.
            // (If we check the `next` pointer before taking the node,
            // we could run into the ABA problem.)
            match self
                .head
                .compare_exchange_weak(node, LOCKED.cast(), Acquire, Relaxed)
            {
                Ok(_) => {
                    // Safety: we swapped the head pointer to LOCKED, so we now
                    // exclusively own this node.
                    let next = unsafe { *(*node).next.get_mut() };
                    // Unlock the list and put the next node back as the head.
                    // We use release ordering here, to make sure that a future
                    // acquire-load of the head pointer still synchronizes with
                    // the release operation that originally stored the pointer
                    // to that node.
                    // (Alternatively, we could use a relaxed swap here.)

                    while BARRIER.load(std::sync::atomic::Ordering::Relaxed) < N_THREADS {}

                    self.head.store(next, Release);
                    return PoolGuard { pool: self, node };
                }
                // The head pointer changed, so we need to try again.
                Err(head) => node = head,
            }
        }
        // No free node currently available. Allocate a new one.
        PoolGuard {
            pool: self,
            node: Box::into_raw(Box::new(Node {
                next: AtomicPtr::new(ptr::null_mut()),
                value: (self.create)(),
            })),
        }
    }
}

impl<'a, T, F> Drop for PoolGuard<'a, T, F> {
    fn drop(&mut self) {
        let mut head = self.pool.head.load(Relaxed);
        loop {
            if head == LOCKED.cast() {
                // Locked! Try again!
                core::hint::spin_loop();
                head = self.pool.head.load(Relaxed);
                continue;
            }
            // Point our node's next pointer to the head of the list.
            // Safety: We haven't given it back to the pool yet, so we still
            // exclusively own this node.
            unsafe { *(*self.node).next.get_mut() = head };
            // Try to put our node back as the head of the list,
            // if the head pointer is (still) the same.
            match self
                .pool
                .head
                .compare_exchange_weak(head, self.node, Release, Relaxed)
            {
                Ok(_) => return,
                Err(p) => head = p,
            }
        }
    }
}

impl<'a, T, F> Deref for PoolGuard<'a, T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        // Safety: The PoolGuard exclusively owns this node.
        unsafe { &(*self.node).value }
    }
}

impl<'a, T, F> DerefMut for PoolGuard<'a, T, F> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: The PoolGuard exclusively owns this node.
        unsafe { &mut (*self.node).value }
    }
}

impl<T, F> Drop for Pool<T, F> {
    fn drop(&mut self) {
        let mut node = *self.head.get_mut();
        while !node.is_null() {
            // Safety: We have exclusive access to the pool now (&mut self),
            // including all the nodes, so there is no need for any synchronization.
            // So, we can just use .get_mut() on the atomics.
            let next = unsafe { *(*node).next.get_mut() };
            // Safety: This pointer came from Box::into_raw, we have exclusive access
            // to the node, and this is the last time this pointer will be used.
            drop(unsafe { Box::from_raw(node) });
            node = next;
        }
    }
}
