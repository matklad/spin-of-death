use std::{os::unix::thread::JoinHandleExt, thread};

#[cfg(not(feature = "os-blocking-getrandom"))]
use getrandom::getrandom;
#[cfg(feature = "os-blocking-getrandom")]
use os_blocking_getrandom::getrandom;

use thread_priority::{
    set_thread_priority, RealtimeThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy,
};

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms))
}

// Pretend that we don't have `getrandom` syscall, to trick `getrandom` crate
// into using a spin-lock protected read from `/dev/urandom`.
#[no_mangle]
pub extern "C" fn syscall(_syscall: u64, _buf: *const u8, _len: usize, _flags: u32) -> isize {
    extern "C" {
        fn __errno_location() -> *mut i32;
    }
    unsafe {
        *__errno_location() = 38; // ENOSYS
    }
    -1
}

// Pretend that reading from `/dev/urandom` blocks
#[no_mangle]
pub extern "C" fn poll(_fds: *const u8, _nfds: usize, _timeout: i32) -> u32 {
    sleep_ms(500);
    1
}

fn main() {
    const N_THREADS: u32 = 512;
    let mut threads = Vec::new();

    // This is a low-priority thread, which will enter a spin-lock protected critical section in `getrandom` crate.
    let t = thread::spawn(|| {
        getrandom(&mut [0; 64]).unwrap();
    });
    set_priority(&t, ThreadPriority::Min);
    threads.push(t);

    // These are high priority threads, which will try to grab a lock held by
    // the first thread.
    for _ in 0..N_THREADS {
        let t = thread::spawn(|| {
            sleep_ms(100); // Make sure that we don't accidently grab the lock first.
            getrandom(&mut [0; 64]).unwrap();
        });
        set_priority(&t, ThreadPriority::Max);
        threads.push(t);
    }

    for thread in threads {
        thread.join().unwrap();
    }
}

fn set_priority(thread: &thread::JoinHandle<()>, priority: ThreadPriority) {
    set_thread_priority(
        thread.as_pthread_t(),
        priority,
        ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::RoundRobin),
    ).unwrap();

}
