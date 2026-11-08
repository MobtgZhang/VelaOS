//! Process management: user mode vs kernel mode, process isolation,
//! per-task address spaces, and process lifecycle.
//!
//! # What this module is
//!
//! It is the boundary between kernel mode (ring 0) and user mode (ring 3).
//! Every user task has its own address space (page tables), its own stack,
//! and runs at CPL 3. The kernel switches to it via `iretq` and returns
//! via `syscall`/`sysret`.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_arch_common::MapError;
use vela_kernel_exec::task::TaskId;
use vela_kernel_syscall::task_space::{FrameSource, TaskSpace, USER_STACK_TOP};
use vela_kernel_syscall::userspace::{LoadError, LoadedImage};

/// A process: a task with its own user address space.
pub struct Process {
    /// The task identifier.
    pub id: TaskId,
    /// The user address space (page tables).
    pub space: Arc<TaskSpace>,
    /// The entry point (user virtual address).
    pub entry: u64,
    /// The user stack top.
    pub stack_top: u64,
    /// Process state.
    pub state: AtomicU32, // 0=created, 1=loaded, 2=running, 3=terminated
    /// Exit code.
    pub exit_code: AtomicU32,
}

impl Process {
    /// Create a new process with a fresh user address space.
    ///
    /// `frames` is accepted for the caller's convenience and for the future
    /// case where a process pre-maps a stack before its image is loaded; the
    /// address space itself is created by [`TaskSpace::new`], which inherits
    /// the kernel's supervisor mappings.
    pub fn new(id: TaskId, _frames: &dyn FrameSource) -> Result<Arc<Self>, MapError> {
        let space = TaskSpace::new();

        Ok(Arc::new(Self {
            id,
            space: Arc::new(space),
            entry: 0,
            stack_top: USER_STACK_TOP,
            state: AtomicU32::new(0),
            exit_code: AtomicU32::new(0),
        }))
    }

    /// Load an ELF program into this process's address space.
    ///
    /// Returns the loader's own error type rather than folding it into
    /// `MapError`: a malformed ELF, a truncated segment and a refused page map
    /// are different failures and must stay distinguishable to the caller.
    pub fn load_program(
        &self,
        image: &[u8],
        frames: &dyn FrameSource,
    ) -> Result<LoadedImage, LoadError> {
        let (_space, loaded) =
            unsafe { vela_kernel_syscall::userspace::load_user_program(image, frames) }?;
        Ok(loaded)
    }

    /// Mark the process as loaded (ready to enter user mode).
    pub fn mark_loaded(&self) {
        self.state.store(1, Ordering::Release);
    }

    /// Mark the process as running (currently in user mode).
    pub fn mark_running(&self) {
        self.state.store(2, Ordering::Release);
    }

    /// Mark the process as terminated.
    pub fn mark_terminated(&self, code: u32) {
        self.state.store(3, Ordering::Release);
        self.exit_code.store(code, Ordering::Release);
    }

    /// Is the process terminated?
    pub fn is_terminated(&self) -> bool {
        self.state.load(Ordering::Acquire) == 3
    }

    /// Get the exit code.
    pub fn exit_code(&self) -> u32 {
        self.exit_code.load(Ordering::Acquire)
    }
}

/// The process table: all processes in the system.
pub struct ProcessTable {
    processes: Mutex<Vec<Arc<Process>>>,
    next_pid: AtomicU32,
}

impl ProcessTable {
    /// Create an empty process table.
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(Vec::new()),
            next_pid: AtomicU32::new(1),
        }
    }

    /// Create a new process and add it to the table.
    pub fn create(&self, frames: &dyn FrameSource) -> Result<Arc<Process>, MapError> {
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        let proc = Process::new(TaskId(pid), frames)?;
        self.processes.lock().push(Arc::clone(&proc));
        Ok(proc)
    }

    /// Find a process by ID.
    pub fn find(&self, pid: u32) -> Option<Arc<Process>> {
        self.processes
            .lock()
            .iter()
            .find(|p| p.id.0 == pid)
            .cloned()
    }

    /// Remove a process from the table.
    pub fn remove(&self, pid: u32) -> Option<Arc<Process>> {
        let mut proc = self.processes.lock();
        let idx = proc.iter().position(|p| p.id.0 == pid)?;
        Some(proc.remove(idx))
    }

    /// How many processes exist.
    pub fn count(&self) -> usize {
        self.processes.lock().len()
    }
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The global process table.
static PROCESS_TABLE: spin::Once<ProcessTable> = spin::Once::new();

/// Get the global process table, installing it on first use.
pub fn process_table() -> &'static ProcessTable {
    PROCESS_TABLE
        .get()
        .expect("process table not installed; kernel/entry must call init_process_table")
}

/// Install the global process table.
///
/// # Safety
///
/// Must be called once during boot, before any process is created.
pub unsafe fn init_process_table() {
    PROCESS_TABLE.call_once(ProcessTable::new);
}

/// Enter user mode at the given entry point with the given stack top.
///
/// # Safety
///
/// Must be called with the process's address space active in CR3, interrupts
/// disabled, and the GDT/TSS installed.
pub unsafe fn enter_user_mode(entry: u64, stack_top: u64) -> ! {
    let context = vela_kernel_syscall::userspace::user_context(entry, stack_top);
    vela_kernel_syscall::userspace::enter_user(context)
}

/// Switch from kernel mode to user mode for `proc`.
///
/// # Safety
///
/// The caller must have the process's address space active.
pub unsafe fn switch_to_user(proc: &Process) -> ! {
    proc.mark_running();
    enter_user_mode(proc.entry, proc.stack_top)
}
