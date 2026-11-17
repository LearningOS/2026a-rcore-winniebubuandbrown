//! Implementation of  [`ProcessControlBlock`]

use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::TaskControlBlock;
use super::{add_task, SignalFlags};
use super::{pid_alloc, PidHandle};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{translated_refmut, MemorySet, KERNEL_SPACE};
use crate::sync::{Condvar, Mutex, Semaphore, UPSafeCell};
use crate::trap::{trap_handler, TrapContext};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

/// Process Control Block
pub struct ProcessControlBlock {
    /// immutable
    pub pid: PidHandle,
    /// mutable
    inner: UPSafeCell<ProcessControlBlockInner>,
}


/// Resource allocation and outstanding requests for deadlock detection.
pub struct DeadlockResource {
    /// Total number of resource units.
    pub capacity: usize,
    /// Units held by each thread, indexed by tid.
    pub allocation: Vec<usize>,
    /// Pending requests from each thread, indexed by tid.
    pub request: Vec<usize>,
}

impl DeadlockResource {
    /// Create a resource record.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            allocation: Vec::new(),
            request: Vec::new(),
        }
    }

    fn ensure_tid(&mut self, tid: usize) {
        if self.allocation.len() <= tid {
            self.allocation.resize(tid + 1, 0);
            self.request.resize(tid + 1, 0);
        }
    }

    pub fn set_request(&mut self, tid: usize, amount: usize) {
        self.ensure_tid(tid);
        self.request[tid] = amount;
    }

    pub fn acquire(&mut self, tid: usize, amount: usize) {
        self.ensure_tid(tid);
        self.request[tid] = 0;
        self.allocation[tid] += amount;
    }

    pub fn release(&mut self, tid: usize, amount: usize) {
        self.ensure_tid(tid);
        self.allocation[tid] = self.allocation[tid].saturating_sub(amount);
    }
}

/// Inner of Process Control Block
pub struct ProcessControlBlockInner {
    /// is zombie?
    pub is_zombie: bool,
    /// memory set(address space)
    pub memory_set: MemorySet,
    /// parent process
    pub parent: Option<Weak<ProcessControlBlock>>,
    /// children process
    pub children: Vec<Arc<ProcessControlBlock>>,
    /// exit code
    pub exit_code: i32,
    /// file descriptor table
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    /// signal flags
    pub signals: SignalFlags,
    /// tasks(also known as threads)
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    /// task resource allocator
    pub task_res_allocator: RecycleAllocator,
    /// mutex list
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    /// semaphore list
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    /// condvar list
    pub condvar_list: Vec<Option<Arc<Condvar>>>,

        /// Whether deadlock detection is enabled.
    pub deadlock_detect: bool,
    /// Resource records used by deadlock detection.
    pub deadlock_resources: Vec<DeadlockResource>,
    /// Resource record index for each mutex.
    pub mutex_resource_ids: Vec<usize>,
    /// Resource record index for each semaphore.
    pub semaphore_resource_ids: Vec<usize>,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    /// get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    /// allocate a new file descriptor
    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }
    /// allocate a new task id
    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }
    /// deallocate a task id
    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }
    /// the count of tasks(threads) in this process
    pub fn thread_count(&self) -> usize {
        self.tasks.len()
    }
    /// get a task with tid in this process
    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }

        /// Register a mutex or semaphore for deadlock detection.
    pub fn register_deadlock_resource(&mut self, capacity: usize) -> usize {
        let id = self.deadlock_resources.len();
        self.deadlock_resources
            .push(DeadlockResource::new(capacity));
        id
    }

    /// Record a pending request and return false if it would deadlock.
    pub fn request_deadlock_resource(&mut self, tid: usize, id: usize) -> bool {
        if id >= self.deadlock_resources.len() {
            return false;
        }

        self.deadlock_resources[id].ensure_tid(tid);
        self.deadlock_resources[id].request[tid] += 1;

        //if self.has_deadlock() {
        if self.deadlock_detect && self.has_deadlock() {
            self.deadlock_resources[id].request[tid] -= 1;
            false
        } else {
            true
        }
    }

    /// Move one resource unit from pending request to allocation.
    pub fn acquire_deadlock_resource(&mut self, tid: usize, id: usize) {
        if let Some(resource) = self.deadlock_resources.get_mut(id) {
            resource.ensure_tid(tid);
            if resource.request[tid] > 0 {
                resource.request[tid] -= 1;
            }
            resource.allocation[tid] += 1;
        }
    }

    /// Release one resource unit.
    pub fn release_deadlock_resource(&mut self, tid: usize, id: usize) {
        if let Some(resource) = self.deadlock_resources.get_mut(id) {
            resource.ensure_tid(tid);
            if resource.allocation[tid] > 0 {
                resource.allocation[tid] -= 1;
            } else if let Some(owner) =
                resource.allocation.iter_mut().find(|count| **count > 0)
            {
                *owner -= 1;
            } else {
                // Semaphores may also be used to signal another thread.
                resource.capacity += 1;
            }
        }
    }

    fn has_deadlock(&self) -> bool {
        let thread_count = self
            .deadlock_resources
            .iter()
            .map(|resource| resource.allocation.len().max(resource.request.len()))
            .max()
            .unwrap_or(0);

        let mut work: Vec<usize> = self
            .deadlock_resources
            .iter()
            .map(|resource| {
                let allocated: usize = resource.allocation.iter().sum();
                resource.capacity.saturating_sub(allocated)
            })
            .collect();

        let mut finish = vec![true; thread_count];
        for tid in 0..thread_count {
            for resource in &self.deadlock_resources {
                let allocated = resource.allocation.get(tid).copied().unwrap_or(0);
                let requested = resource.request.get(tid).copied().unwrap_or(0);
                if allocated != 0 || requested != 0 {
                    finish[tid] = false;
                    break;
                }
            }
        }

        loop {
            let mut progressed = false;
            for tid in 0..thread_count {
                if finish[tid] {
                    continue;
                }

                let can_finish = self
                    .deadlock_resources
                    .iter()
                    .enumerate()
                    .all(|(id, resource)| {
                        resource.request.get(tid).copied().unwrap_or(0) <= work[id]
                    });

                if can_finish {
                    for (id, resource) in self.deadlock_resources.iter().enumerate() {
                        work[id] += resource.allocation.get(tid).copied().unwrap_or(0);
                    }
                    finish[tid] = true;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        finish.iter().any(|finished| !finished)
    }
}

impl ProcessControlBlock {
    /// inner_exclusive_access
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// new process from elf file
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    deadlock_detect: false,
                    deadlock_resources: Vec::new(),
                    mutex_resource_ids: Vec::new(),
                    semaphore_resource_ids: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>) {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        // memory_set with elf program headers/trampoline/trap context/user stack
        trace!("kernel: exec .. MemorySet::from_elf");
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        let new_token = memory_set.token();
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        self.inner_exclusive_access().memory_set = memory_set;
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        trace!("kernel: exec .. alloc user resource for main thread again");
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        // push arguments on user stack
        trace!("kernel: exec .. push arguments on user stack");
        let mut user_sp = task_inner.res.as_mut().unwrap().ustack_top();
        user_sp -= (args.len() + 1) * core::mem::size_of::<usize>();
        let argv_base = user_sp;
        let mut argv: Vec<_> = (0..=args.len())
            .map(|arg| {
                translated_refmut(
                    new_token,
                    (argv_base + arg * core::mem::size_of::<usize>()) as *mut usize,
                )
            })
            .collect();
        *argv[args.len()] = 0;
        for i in 0..args.len() {
            user_sp -= args[i].len() + 1;
            *argv[i] = user_sp;
            let mut p = user_sp;
            for c in args[i].as_bytes() {
                *translated_refmut(new_token, p as *mut u8) = *c;
                p += 1;
            }
            *translated_refmut(new_token, p as *mut u8) = 0;
        }
        // make the user_sp aligned to 8B for k210 platform
        user_sp -= user_sp % core::mem::size_of::<usize>();
        // initialize trap_cx
        trace!("kernel: exec .. initialize trap_cx");
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        *task_inner.get_trap_cx() = trap_cx;
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        trace!("kernel: fork");
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    deadlock_detect: false,
                    deadlock_resources: Vec::new(),
                    mutex_resource_ids: Vec::new(),
                    semaphore_resource_ids: Vec::new(),
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }
    /// get pid
    pub fn getpid(&self) -> usize {
        self.pid.0
    }
}
