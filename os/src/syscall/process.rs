//! Process management syscalls
use crate::config::PAGE_SIZE;
///use crate::task::{change_program_brk, exit_current_and_run_next, suspend_current_and_run_next};

use crate::{
    //mm::{translated_byte_buffer,PageTable, PTEFlags, VirtAddr},
    mm::{
    translated_byte_buffer,
    MapPermission,
    PageTable,
    PTEFlags,
    VirtAddr,   
    },
    //task::{change_program_brk, current_user_token, exit_current_and_run_next, suspend_current_and_run_next,},
    task::{
    change_program_brk,
    current_syscall_count,
    current_user_token,
    exit_current_and_run_next,
    mmap_current,
    munmap_current,
    suspend_current_and_run_next,
    },
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
///pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
   /// trace!("kernel: sys_get_time");
    ///-1
//}

pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");

    let us = get_time_us();
    let value = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    let src = unsafe {
        core::slice::from_raw_parts(
            &value as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    let mut buffers =
        translated_byte_buffer(current_user_token(), ts as *const u8, src.len());

    let mut offset = 0;
    for buffer in buffers.iter_mut() {
        let len = buffer.len();
        buffer.copy_from_slice(&src[offset..offset + len]);
        offset += len;
    }
    0
}

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
//pub fn sys_trace(_trace_request: usize, _id: usize, _data: usize) -> isize {
  //  trace!("kernel: sys_trace");
    //-1
//}

pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");

    if trace_request == 2 {
        return current_syscall_count(id) as isize;
    }

    let page_table = PageTable::from_token(current_user_token());
    let va = VirtAddr::from(id);

    let pte = match page_table.translate(va.floor()) {
        Some(pte) if pte.is_valid() && pte.flags().contains(PTEFlags::U) => pte,
        _ => return -1,
    };

    let byte = &mut pte.ppn().get_bytes_array()[va.page_offset()];

    match trace_request {
        0 if pte.readable() => *byte as isize,
        1 if pte.writable() => {
            *byte = data as u8;
            0
        }
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
//pub fn sys_mmap(_start: usize, _len: usize, _port: usize) -> isize {
  //  trace!("kernel: sys_mmap NOT IMPLEMENTED YET!");
   // -1
//}
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");

    //if start % PAGE_SIZE != 0 || len == 0 || port & !0x7 != 0 {
    //    return -1;
    //}

    if start % PAGE_SIZE != 0
        || len == 0
        || port == 0
        || port & !0x7 != 0
    {
        return -1;
    }

    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };

    let permission =
        MapPermission::from_bits((port << 1) as u8).unwrap() | MapPermission::U;

    if mmap_current(start.into(), end.into(), permission) {
        0
    } else {
        -1
    }
}

// YOUR JOB: Implement munmap.
//pub fn sys_munmap(_start: usize, _len: usize) -> isize {
   // trace!("kernel: sys_munmap NOT IMPLEMENTED YET!");
    //-1
//}
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");

    if start % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }

    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };

    if munmap_current(start.into(), end.into()) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
