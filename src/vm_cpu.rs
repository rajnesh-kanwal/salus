// Copyright (c) 2022 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

use core::arch::global_asm;
use core::{marker::PhantomData, mem::size_of, ptr, ptr::NonNull};
use drivers::{imsic::*, CpuId, CpuInfo, MAX_CPUS};
use memoffset::offset_of;
use page_tracking::collections::PageBox;
use page_tracking::TlbVersion;
use riscv_page_tables::GuestStagePagingMode;
use riscv_pages::{GuestPhysAddr, GuestVirtAddr, PageOwnerId, PageSize, RawAddr};
use riscv_regs::*;
use sbi::{self, SbiMessage, SbiReturnType};
use spin::{Mutex, MutexGuard, Once, RwLock};

use crate::smp::PerCpu;
use crate::vm::{MmioOpcode, MmioOperation, VmExitCause};
use crate::vm_id::VmId;
use crate::vm_interrupts::{self, VmCpuExtInterrupts};
use crate::vm_pages::{ActiveVmPages, FinalizedVmPages, PinnedPages};
use crate::vm_pmu::VmPmuState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadVmCpuId,
    VmCpuExists,
    VmCpuNotFound,
    VmCpuRunning,
    VmCpuOff,
    VmCpuAlreadyPowered,
    VmCpuBlocked,
    WrongAddressSpace,
    InvalidSharedStatePtr,
    InsufficientSharedStatePages,
    NoImsicVirtualization,
    ImsicLocationAlreadySet,
    VmCpuNotBound,
    Binding(vm_interrupts::Error),
    Rebinding(vm_interrupts::Error),
    Unbinding(vm_interrupts::Error),
    AllowingInterrupt(vm_interrupts::Error),
    DenyingInterrupt(vm_interrupts::Error),
    InjectingInterrupt(vm_interrupts::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

/// The maximum number of vCPUs supported by a VM.
pub const VM_CPUS_MAX: usize = MAX_CPUS;

/// Host GPR and CSR state which must be saved/restored when entering/exiting virtualization.
#[derive(Default)]
#[repr(C)]
struct HostCpuState {
    gprs: GeneralPurposeRegisters,
    sstatus: u64,
    hstatus: u64,
    scounteren: u64,
    stvec: u64,
    sscratch: u64,
}

/// Guest GPR and CSR state which must be saved/restored when exiting/entering virtualization.
#[derive(Default)]
#[repr(C)]
struct GuestCpuState {
    gprs: GeneralPurposeRegisters,
    fprs: FloatingPointRegisters,
    vprs: VectorRegisters,
    fcsr: u64,
    sstatus: u64,
    hstatus: u64,
    scounteren: u64,
    sepc: u64,

    vstart: u64,
    vcsr: u64,
    vtype: u64,
    vl: u64,
}

/// The CSRs that are only in effect when virtualization is enabled (V=1) and must be saved and
/// restored whenever we switch between VMs.
#[derive(Default)]
#[repr(C)]
struct GuestVCpuState {
    htimedelta: u64,
    vsstatus: u64,
    vsie: u64,
    vstvec: u64,
    vsscratch: u64,
    vsepc: u64,
    vscause: u64,
    vstval: u64,
    vsatp: u64,
    vstimecmp: u64,
}

/// CSRs written on an exit from virtualization that are used by the host to determine the cause of
/// the trap.
#[derive(Default, Clone)]
#[repr(C)]
pub struct VmCpuTrapState {
    pub scause: u64,
    pub stval: u64,
    pub htval: u64,
    pub htinst: u64,
}

/// (v)CPU register state that must be saved or restored when entering/exiting a VM or switching
/// between VMs.
#[derive(Default)]
#[repr(C)]
struct VmCpuRegisters {
    // CPU state that's shared between our's and the guest's execution environment. Saved/restored
    // when entering/exiting a VM.
    host_regs: HostCpuState,
    guest_regs: GuestCpuState,

    // CPU state that only applies when V=1, e.g. the VS-level CSRs. Saved/restored on activation of
    // the vCPU.
    guest_vcpu_csrs: GuestVCpuState,

    // Read on VM exit.
    trap_csrs: VmCpuTrapState,
}

// The vCPU context switch, defined in guest.S
extern "C" {
    fn _run_guest(state: *mut VmCpuRegisters);
    fn _save_fp(state: *mut VmCpuRegisters);
    fn _restore_fp(state: *mut VmCpuRegisters);
}

extern "C" {
    fn _restore_vector(state: *mut VmCpuRegisters);
    fn _save_vector(state: *mut VmCpuRegisters);
}

#[allow(dead_code)]
const fn host_gpr_offset(index: GprIndex) -> usize {
    offset_of!(VmCpuRegisters, host_regs)
        + offset_of!(HostCpuState, gprs)
        + (index as usize) * size_of::<u64>()
}

#[allow(dead_code)]
const fn guest_gpr_offset(index: GprIndex) -> usize {
    offset_of!(VmCpuRegisters, guest_regs)
        + offset_of!(GuestCpuState, gprs)
        + (index as usize) * size_of::<u64>()
}

const fn guest_fpr_offset(index: usize) -> usize {
    offset_of!(VmCpuRegisters, guest_regs)
        + offset_of!(GuestCpuState, fprs)
        + index * size_of::<u64>()
}

const fn guest_vpr_offset(index: usize) -> usize {
    offset_of!(VmCpuRegisters, guest_regs)
        + offset_of!(GuestCpuState, vprs)
        + index * size_of::<riscv_regs::VectorRegister>()
}

macro_rules! host_csr_offset {
    ($reg:tt) => {
        offset_of!(VmCpuRegisters, host_regs) + offset_of!(HostCpuState, $reg)
    };
}

macro_rules! guest_csr_offset {
    ($reg:tt) => {
        offset_of!(VmCpuRegisters, guest_regs) + offset_of!(GuestCpuState, $reg)
    };
}

global_asm!(
    include_str!("guest.S"),
    host_ra = const host_gpr_offset(GprIndex::RA),
    host_gp = const host_gpr_offset(GprIndex::GP),
    host_tp = const host_gpr_offset(GprIndex::TP),
    host_s0 = const host_gpr_offset(GprIndex::S0),
    host_s1 = const host_gpr_offset(GprIndex::S1),
    host_a1 = const host_gpr_offset(GprIndex::A1),
    host_a2 = const host_gpr_offset(GprIndex::A2),
    host_a3 = const host_gpr_offset(GprIndex::A3),
    host_a4 = const host_gpr_offset(GprIndex::A4),
    host_a5 = const host_gpr_offset(GprIndex::A5),
    host_a6 = const host_gpr_offset(GprIndex::A6),
    host_a7 = const host_gpr_offset(GprIndex::A7),
    host_s2 = const host_gpr_offset(GprIndex::S2),
    host_s3 = const host_gpr_offset(GprIndex::S3),
    host_s4 = const host_gpr_offset(GprIndex::S4),
    host_s5 = const host_gpr_offset(GprIndex::S5),
    host_s6 = const host_gpr_offset(GprIndex::S6),
    host_s7 = const host_gpr_offset(GprIndex::S7),
    host_s8 = const host_gpr_offset(GprIndex::S8),
    host_s9 = const host_gpr_offset(GprIndex::S9),
    host_s10 = const host_gpr_offset(GprIndex::S10),
    host_s11 = const host_gpr_offset(GprIndex::S11),
    host_sp = const host_gpr_offset(GprIndex::SP),
    host_sstatus = const host_csr_offset!(sstatus),
    host_hstatus = const host_csr_offset!(hstatus),
    host_scounteren = const host_csr_offset!(scounteren),
    host_stvec = const host_csr_offset!(stvec),
    host_sscratch = const host_csr_offset!(sscratch),
    guest_ra = const guest_gpr_offset(GprIndex::RA),
    guest_gp = const guest_gpr_offset(GprIndex::GP),
    guest_tp = const guest_gpr_offset(GprIndex::TP),
    guest_s0 = const guest_gpr_offset(GprIndex::S0),
    guest_s1 = const guest_gpr_offset(GprIndex::S1),
    guest_a0 = const guest_gpr_offset(GprIndex::A0),
    guest_a1 = const guest_gpr_offset(GprIndex::A1),
    guest_a2 = const guest_gpr_offset(GprIndex::A2),
    guest_a3 = const guest_gpr_offset(GprIndex::A3),
    guest_a4 = const guest_gpr_offset(GprIndex::A4),
    guest_a5 = const guest_gpr_offset(GprIndex::A5),
    guest_a6 = const guest_gpr_offset(GprIndex::A6),
    guest_a7 = const guest_gpr_offset(GprIndex::A7),
    guest_s2 = const guest_gpr_offset(GprIndex::S2),
    guest_s3 = const guest_gpr_offset(GprIndex::S3),
    guest_s4 = const guest_gpr_offset(GprIndex::S4),
    guest_s5 = const guest_gpr_offset(GprIndex::S5),
    guest_s6 = const guest_gpr_offset(GprIndex::S6),
    guest_s7 = const guest_gpr_offset(GprIndex::S7),
    guest_s8 = const guest_gpr_offset(GprIndex::S8),
    guest_s9 = const guest_gpr_offset(GprIndex::S9),
    guest_s10 = const guest_gpr_offset(GprIndex::S10),
    guest_s11 = const guest_gpr_offset(GprIndex::S11),
    guest_t0 = const guest_gpr_offset(GprIndex::T0),
    guest_t1 = const guest_gpr_offset(GprIndex::T1),
    guest_t2 = const guest_gpr_offset(GprIndex::T2),
    guest_t3 = const guest_gpr_offset(GprIndex::T3),
    guest_t4 = const guest_gpr_offset(GprIndex::T4),
    guest_t5 = const guest_gpr_offset(GprIndex::T5),
    guest_t6 = const guest_gpr_offset(GprIndex::T6),
    guest_sp = const guest_gpr_offset(GprIndex::SP),
    guest_f0 = const guest_fpr_offset(0),
    guest_f1 = const guest_fpr_offset(1),
    guest_f2 = const guest_fpr_offset(2),
    guest_f3 = const guest_fpr_offset(3),
    guest_f4 = const guest_fpr_offset(4),
    guest_f5 = const guest_fpr_offset(5),
    guest_f6 = const guest_fpr_offset(6),
    guest_f7 = const guest_fpr_offset(7),
    guest_f8 = const guest_fpr_offset(8),
    guest_f9 = const guest_fpr_offset(9),
    guest_f10 = const guest_fpr_offset(10),
    guest_f11 = const guest_fpr_offset(11),
    guest_f12 = const guest_fpr_offset(12),
    guest_f13 = const guest_fpr_offset(13),
    guest_f14 = const guest_fpr_offset(14),
    guest_f15 = const guest_fpr_offset(15),
    guest_f16 = const guest_fpr_offset(16),
    guest_f17 = const guest_fpr_offset(17),
    guest_f18 = const guest_fpr_offset(18),
    guest_f19 = const guest_fpr_offset(19),
    guest_f20 = const guest_fpr_offset(20),
    guest_f21 = const guest_fpr_offset(21),
    guest_f22 = const guest_fpr_offset(22),
    guest_f23 = const guest_fpr_offset(23),
    guest_f24 = const guest_fpr_offset(24),
    guest_f25 = const guest_fpr_offset(25),
    guest_f26 = const guest_fpr_offset(26),
    guest_f27 = const guest_fpr_offset(27),
    guest_f28 = const guest_fpr_offset(28),
    guest_f29 = const guest_fpr_offset(29),
    guest_f30 = const guest_fpr_offset(30),
    guest_f31 = const guest_fpr_offset(31),
    guest_fcsr = const guest_csr_offset!(fcsr),
    guest_v0 = const guest_vpr_offset(0),
    guest_v8 = const guest_vpr_offset(8),
    guest_v16 = const guest_vpr_offset(16),
    guest_v24 = const guest_vpr_offset(24),
    guest_vstart = const guest_csr_offset!(vstart),
    guest_vcsr = const guest_csr_offset!(vcsr),
    guest_vtype = const guest_csr_offset!(vtype),
    guest_vl = const guest_csr_offset!(vl),
    guest_sstatus = const guest_csr_offset!(sstatus),
    guest_hstatus = const guest_csr_offset!(hstatus),
    guest_scounteren = const guest_csr_offset!(scounteren),
    guest_sepc = const guest_csr_offset!(sepc),
    sstatus_fs_dirty = const sstatus::fs::Dirty.value,
    sstatus_vs_enable = const sstatus::vs::Initial.value,
);

/// Defines the structure of the vCPU shared-memory state area.
#[derive(Default)]
pub struct VmCpuSharedState {
    gprs: sbi::Gprs,
    s_csrs: sbi::SupervisorCsrs,
    hs_csrs: sbi::HypervisorCsrs,
    sstc_csrs: sbi::SstcCsrs,
}

/// Defines the layout of `VmCpuSharedState` in terms of `RegisterSetLocation`s.
pub const VM_CPU_SHARED_LAYOUT: &[sbi::RegisterSetLocation] = &[
    sbi::RegisterSetLocation {
        id: sbi::RegisterSetId::Gprs as u16,
        offset: offset_of!(VmCpuSharedState, gprs) as u16,
    },
    sbi::RegisterSetLocation {
        id: sbi::RegisterSetId::SupervisorCsrs as u16,
        offset: offset_of!(VmCpuSharedState, s_csrs) as u16,
    },
    sbi::RegisterSetLocation {
        id: sbi::RegisterSetId::HypervisorCsrs as u16,
        offset: offset_of!(VmCpuSharedState, hs_csrs) as u16,
    },
    sbi::RegisterSetLocation {
        id: sbi::RegisterSetId::SstcCsrs as u16,
        offset: offset_of!(VmCpuSharedState, sstc_csrs) as u16,
    },
];

/// The number of pages required for `VmCpuSharedState`.
pub const VM_CPU_SHARED_PAGES: u64 = PageSize::num_4k_pages(size_of::<VmCpuSharedState>() as u64);

/// Provides volatile accessors to a `VmCpuSharedState`.
pub struct VmCpuSharedStateRef<'a> {
    ptr: *mut VmCpuSharedState,
    _lifetime: PhantomData<&'a VmCpuSharedState>,
}

// Defines volatile accessors to idividual fields in `VmCpuSharedState`.
macro_rules! define_accessors {
    ($regset:ident, $field:ident, $get:ident, $set:ident) => {
        /// Gets $field in the shared-memory state area.
        #[allow(dead_code)]
        pub fn $get(&self) -> u64 {
            // Safety: The caller guaranteed at construction that `ptr` points to a valid
            // VmCpuSharedState.
            unsafe { ptr::addr_of!((*self.ptr).$regset.$field).read_volatile() }
        }

        /// Sets $field in the shared-memory state area.
        #[allow(dead_code)]
        pub fn $set(&self, val: u64) {
            // Safety: The caller guaranteed at construction that `ptr` points to a valid
            // VmCpuSharedState.
            unsafe { ptr::addr_of_mut!((*self.ptr).$regset.$field).write_volatile(val) };
        }
    };
}

impl<'a> VmCpuSharedStateRef<'a> {
    /// Creates a new `VvmCpuSharedStateRef` from a raw pointer to a `VmCpuSharedState`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `ptr` is suitably aligned and points to a `TvmCpuSharedState`
    /// structure that is valid for the lifetime `'a`.
    pub unsafe fn new(ptr: *mut VmCpuSharedState) -> Self {
        Self {
            ptr,
            _lifetime: PhantomData,
        }
    }

    define_accessors! {s_csrs, sepc, sepc, set_sepc}
    define_accessors! {s_csrs, scause, scause, set_scause}
    define_accessors! {s_csrs, stval, stval, set_stval}
    define_accessors! {hs_csrs, htval, htval, set_htval}
    define_accessors! {hs_csrs, htinst, htinst, set_htinst}
    define_accessors! {sstc_csrs, vstimecmp, vstimecmp, set_vstimecmp}

    /// Reads the general purpose register at `index`.
    pub fn gpr(&self, index: GprIndex) -> u64 {
        // Safety: `index` is guaranteed to be a valid GPR index and the caller guaranteed at
        // construction that `ptr` points to a valid `VmCpuSharedState`.
        unsafe { ptr::addr_of!((*self.ptr).gprs.0[index as usize]).read_volatile() }
    }

    /// Writes the general purpose register at `index`.
    pub fn set_gpr(&self, index: GprIndex, val: u64) {
        // Safety: `index` is guaranteed to be a valid GPR index and the caller guaranteed at
        // construction that `ptr` points to a valid `VmCpuSharedState`.
        unsafe { ptr::addr_of_mut!((*self.ptr).gprs.0[index as usize]).write_volatile(val) };
    }
}

/// Wrapper for the shared-memory state area used to communicate vCPU state with the host.
pub struct VmCpuSharedArea {
    ptr: NonNull<VmCpuSharedState>,
    // Optional since we might be sharing with the hypervisor in the host VM case.
    _pin: Option<PinnedPages>,
}

impl VmCpuSharedArea {
    /// Creates a new `VmCpuSharedArea` using the `VmCpuSharedState` struct referred to by `ptr`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `ptr` is suitably aligned, points to an allocated block of
    /// memory that is at least as large as `VmCpuSharedState`, and is valid for reads and writes
    /// for the lifetime of this structure. The caller must only access the memory referred to
    /// by `ptr` using volatile accessors.
    pub unsafe fn new(ptr: *mut VmCpuSharedState) -> Result<Self> {
        Ok(Self {
            ptr: NonNull::new(ptr).ok_or(Error::InvalidSharedStatePtr)?,
            _pin: None,
        })
    }

    /// Creates a new `VmCpuSharedArea` using the pinned pages referred to by `pages`.
    pub fn from_pinned_pages(pages: PinnedPages) -> Result<Self> {
        // Make sure the pin actually covers the size of the structure.
        if pages.range().num_pages() < VM_CPU_SHARED_PAGES {
            return Err(Error::InsufficientSharedStatePages);
        }
        let ptr = pages.range().base().bits() as *mut VmCpuSharedState;
        Ok(Self {
            ptr: NonNull::new(ptr).ok_or(Error::InvalidSharedStatePtr)?,
            _pin: Some(pages),
        })
    }

    // Updates the shared state buffer for an ECALL exit from the given SBI message.
    fn update_with_ecall_exit(&self, msg: SbiMessage) {
        let shared = self.as_ref();
        shared.set_gpr(GprIndex::A0, msg.a0());
        shared.set_gpr(GprIndex::A1, msg.a1());
        shared.set_gpr(GprIndex::A2, msg.a2());
        shared.set_gpr(GprIndex::A3, msg.a3());
        shared.set_gpr(GprIndex::A4, msg.a4());
        shared.set_gpr(GprIndex::A5, msg.a5());
        shared.set_gpr(GprIndex::A6, msg.a6());
        shared.set_gpr(GprIndex::A7, msg.a7());
        shared.set_stval(0);
        shared.set_htval(0);
        shared.set_htinst(0);
        shared.set_scause(Exception::VirtualSupervisorEnvCall as u64);
    }

    // Updates the shared state buffer for a guest page fault exit at the given address.
    fn update_with_pf_exit(&self, exception: Exception, addr: GuestPhysAddr) {
        let shared = self.as_ref();
        shared.set_stval(addr.bits() & 0x3);
        shared.set_htval(addr.bits() >> 2);
        shared.set_htinst(0);
        shared.set_scause(exception as u64);
    }

    // Updates the shared state buffer for a virtual instruction exception caused by the given
    // instruction.
    fn update_with_vi_exit(&self, inst: u64) {
        let shared = self.as_ref();
        // Note that this is technically not spec-compliant as the privileged spec only states
        // that illegal instruction exceptions may write the faulting instruction to *TVAL CSRs.
        shared.set_stval(inst);
        shared.set_htval(0);
        shared.set_htinst(0);
        shared.set_scause(Exception::VirtualInstruction as u64);
    }

    // Updates the shared state buffer for an unhandled exit due to `scause`.
    fn update_with_unhandled_exit(&self, scause: u64) {
        let shared = self.as_ref();
        shared.set_stval(0);
        shared.set_htval(0);
        shared.set_htinst(0);
        shared.set_scause(scause);
    }

    // Returns the wrapped shared state buffer as a `VmCpuSharedRef`.
    fn as_ref(&self) -> VmCpuSharedStateRef {
        // Safety: We've validated at construction that self.ptr points to a valid
        // `VmCpuSharedState`.
        unsafe { VmCpuSharedStateRef::new(self.ptr.as_ptr()) }
    }
}

/// Identifies the reason for a trap taken from a vCPU.
pub enum VmCpuTrap {
    /// ECALLs from VS mode.
    Ecall(Option<SbiMessage>),
    /// G-stage page faults.
    PageFault {
        exception: Exception,
        fault_addr: GuestPhysAddr,
        fault_pc: GuestVirtAddr,
        priv_level: PrivilegeLevel,
    },
    /// Instruction emulation trap.
    VirtualInstruction {
        fault_pc: GuestVirtAddr,
        priv_level: PrivilegeLevel,
    },
    /// An exception which we expected to handle directly at VS, but trapped to HS instead.
    DelegatedException { exception: Exception, stval: u64 },
    /// Everything else that we currently don't or can't handle.
    Other(VmCpuTrapState),
    // TODO: Add other exit causes as needed.
}

// The VMID and TLB version last used by a vCPU.
struct PrevTlb {
    cpu: CpuId,
    vmid: VmId,
    tlb_version: TlbVersion,
}

// The architectural state of a vCPU.
struct VmCpuArchState {
    regs: VmCpuRegisters,
    pmu: VmPmuState,
    prev_tlb: Option<PrevTlb>,
    pending_mmio_op: Option<MmioOperation>,
}

impl VmCpuArchState {
    // Sets up the architectural state for a new vCPU.
    fn new(guest_id: PageOwnerId) -> Self {
        let mut regs = VmCpuRegisters::default();

        let mut hstatus = LocalRegisterCopy::<u64, hstatus::Register>::new(0);
        hstatus.modify(hstatus::spv.val(1));
        hstatus.modify(hstatus::spvp::Supervisor);
        if !guest_id.is_host() {
            // Trap on WFI for non-host VMs. Trapping WFI for the host is pointless since all we'd do
            // in the hypervisor is WFI ourselves.
            hstatus.modify(hstatus::vtw.val(1));
        }
        regs.guest_regs.hstatus = hstatus.get();

        let mut sstatus = LocalRegisterCopy::<u64, sstatus::Register>::new(0);
        sstatus.modify(sstatus::spp::Supervisor);
        sstatus.modify(sstatus::fs::Initial);
        if CpuInfo::get().has_vector() {
            sstatus.modify(sstatus::vs::Initial);
        }
        regs.guest_regs.sstatus = sstatus.get();

        let mut scounteren = LocalRegisterCopy::<u64, scounteren::Register>::new(0);
        scounteren.modify(scounteren::cycle.val(1));
        scounteren.modify(scounteren::time.val(1));
        scounteren.modify(scounteren::instret.val(1));
        regs.guest_regs.scounteren = scounteren.get();

        Self {
            regs,
            pmu: VmPmuState::default(),
            prev_tlb: None,
            pending_mmio_op: None,
        }
    }
}

// Sets the status on dropping depending on the state of next_status.
// Used by ActiveVmCpu sto set the Vcpu's state on de-activation.
struct StatusSet<'vcpu> {
    vcpu: &'vcpu VmCpu,
    next_status: VmCpuStatus,
}

impl<'vcpu> StatusSet<'vcpu> {
    fn new(vcpu: &'vcpu VmCpu) -> Self {
        // vCPUs return to the "Runnable" state by default.
        Self {
            vcpu,
            next_status: VmCpuStatus::Runnable,
        }
    }
}

impl<'vcpu> Drop for StatusSet<'vcpu> {
    fn drop(&mut self) {
        let mut status = self.vcpu.status.write();
        assert_eq!(*status, VmCpuStatus::Running);
        *status = self.next_status;
    }
}

/// An activated vCPU. A vCPU in this state has entered the VM's address space and is ready to run.
pub struct ActiveVmCpu<'vcpu, 'pages, 'prev, T: GuestStagePagingMode> {
    vcpu: &'vcpu VmCpu,
    // We hold a lock on vcpu.arch for the lifetime of this object to avoid having to
    // repeatedly acquire it for every register modification. Must be dropped before `status_set`,
    // so declared first.
    arch: MutexGuard<'vcpu, VmCpuArchState>,
    vm_pages: FinalizedVmPages<'pages, T>,
    // `None` if this vCPU is itself running a child vCPU. Restored when the child vCPU exits.
    active_pages: Option<ActiveVmPages<'pages, T>>,
    // The parent vCPU which activated us.
    parent_vcpu: Option<&'prev mut dyn VmCpuSaveState>,
    // Important drop order, status_set must come _after_ `arch` to maintain lock ordering.
    // on drop StatusSet takes the status lock.
    status_set: StatusSet<'vcpu>,
}

impl<'vcpu, 'pages, 'prev, T: GuestStagePagingMode> ActiveVmCpu<'vcpu, 'pages, 'prev, T> {
    // Restores and activates the vCPU state from `vcpu`, with the VM address space represented by
    // `vm_pages`.
    fn restore_from(
        vcpu: &'vcpu VmCpu,
        vm_pages: FinalizedVmPages<'pages, T>,
        parent_vcpu: Option<&'prev mut ActiveVmCpu<T>>,
    ) -> Result<Self> {
        let mut arch = vcpu.arch.lock();
        // If we're running on a new CPU, then any previous VMID or TLB version we used is inavlid.
        if let Some(ref prev_tlb) = arch.prev_tlb && prev_tlb.cpu != PerCpu::this_cpu().cpu_id() {
            arch.prev_tlb = None;
        }

        // We store the parent vCPU as a trait object as a means of type-erasure for ActiveVmCpu's
        // inner lifetimes.
        let mut parent_vcpu = parent_vcpu.map(|p| p as &mut dyn VmCpuSaveState);

        // Save the state of the parent (if any) and swap in ours.
        if let Some(ref mut p) = parent_vcpu {
            p.save();
        }
        let mut active_vcpu = Self {
            vcpu,
            arch,
            vm_pages,
            active_pages: None,
            parent_vcpu,
            status_set: StatusSet::new(vcpu),
        };
        active_vcpu.restore();

        Ok(active_vcpu)
    }

    /// Runs this vCPU until it traps.
    pub fn run(&mut self) -> VmCpuTrap {
        self.complete_pending_mmio_op();

        // TODO, HGEIE programinng:
        //  - Track which guests the host wants interrupts from (by trapping HGEIE accesses from
        //    VS level) and update HGEIE[2:] appropriately.
        //  - If this is the host: clear HGEIE[1] on entry; inject SGEI into host VM if we receive
        //    any SGEI at HS level.
        //  - If this is a guest: set HGEIE[1] on entry; switch to the host VM for any SGEI that
        //    occur, injecting an SEI for the host interrupts and SGEI for guest VM interrupts.

        let has_vector = CpuInfo::get().has_vector();
        let guest_id = self.vcpu.guest_id;
        let regs = &mut self.arch.regs;

        unsafe {
            // Safe since _restore_vector() only reads within the bounds of the vector register
            // state in VmCpuRegisters.
            if has_vector {
                _restore_vector(regs);
            }

            // Safe since _restore_fp() only reads within the bounds of the floating point
            // register state in VmCpuRegisters.
            _restore_fp(regs);

            // Safe to run the guest as it only touches memory assigned to it by being owned
            // by its page table.
            _run_guest(regs);
        }

        // Save off the trap information.
        regs.trap_csrs.scause = CSR.scause.get();
        regs.trap_csrs.stval = CSR.stval.get();
        regs.trap_csrs.htval = CSR.htval.get();
        regs.trap_csrs.htinst = CSR.htinst.get();

        // Check if FPU state needs to be saved.
        let mut sstatus = LocalRegisterCopy::new(regs.guest_regs.sstatus);
        if sstatus.matches_all(sstatus::fs::Dirty) {
            // Safe since _save_fp() only writes within the bounds of the floating point register
            // state in VmCpuRegisters.
            unsafe { _save_fp(regs) };
            sstatus.modify(sstatus::fs::Clean);
        }
        // Check if vector state needs to be saved
        if has_vector && sstatus.matches_all(sstatus::vs::Dirty) {
            // Safe since _save_vector() only writes within the bounds of the vector register
            // state in VmCpuRegisters.
            unsafe { _save_vector(regs) };
            sstatus.modify(sstatus::vs::Clean)
        }
        regs.guest_regs.sstatus = sstatus.get();

        // Determine the exit cause from the trap CSRs.
        use Exception::*;
        match Trap::from_scause(regs.trap_csrs.scause).unwrap() {
            Trap::Exception(VirtualSupervisorEnvCall) => {
                let sbi_msg = SbiMessage::from_regs(regs.guest_regs.gprs.a_regs()).ok();
                VmCpuTrap::Ecall(sbi_msg)
            }
            Trap::Exception(GuestInstructionPageFault)
            | Trap::Exception(GuestLoadPageFault)
            | Trap::Exception(GuestStorePageFault) => {
                let fault_addr = RawAddr::guest(
                    regs.trap_csrs.htval << 2 | regs.trap_csrs.stval & 0x03,
                    guest_id,
                );
                VmCpuTrap::PageFault {
                    exception: Exception::from_scause_reason(regs.trap_csrs.scause).unwrap(),
                    fault_addr,
                    // Note that this address is not necessarily guest virtual as the guest may or
                    // may not have 1st-stage translation enabled in VSATP. We still use GuestVirtAddr
                    // here though to distinguish it from addresses (e.g. in HTVAL, or passed via a
                    // TEECALL) which are exclusively guest-physical. Furthermore we only access guest
                    // instructions via the HLVX instruction, which will take the VSATP translation
                    // mode into account.
                    fault_pc: RawAddr::guest_virt(regs.guest_regs.sepc, guest_id),
                    priv_level: PrivilegeLevel::from_hstatus(regs.guest_regs.hstatus),
                }
            }
            Trap::Exception(VirtualInstruction) => {
                VmCpuTrap::VirtualInstruction {
                    // See above re: this address being guest virtual.
                    fault_pc: RawAddr::guest_virt(regs.guest_regs.sepc, guest_id),
                    priv_level: PrivilegeLevel::from_hstatus(regs.guest_regs.hstatus),
                }
            }
            Trap::Exception(e) => {
                if e.to_hedeleg_field()
                    .map_or(false, |f| CSR.hedeleg.matches_any(f))
                {
                    // Even if we intended to delegate this exception it might not be set in
                    // medeleg, in which case firmware may send it our way instead.
                    VmCpuTrap::DelegatedException {
                        exception: e,
                        stval: regs.trap_csrs.stval,
                    }
                } else {
                    VmCpuTrap::Other(regs.trap_csrs.clone())
                }
            }
            _ => VmCpuTrap::Other(regs.trap_csrs.clone()),
        }
    }

    // Rewrites `mmio_op` as a transformed load or store instruction to/from A0 as would be written
    // to the HTINST CSR.
    fn mmio_op_to_htinst(mmio_op: MmioOperation) -> u64 {
        use MmioOpcode::*;
        // Get the base instruction for the operation.
        let htinst_base = match mmio_op.opcode() {
            Store8 => MATCH_SB,
            Store16 => MATCH_SH,
            Store32 => MATCH_SW,
            Store64 => MATCH_SD,
            Load8 => MATCH_LB,
            Load16 => MATCH_LH,
            Load32 => MATCH_LW,
            Load8U => MATCH_LBU,
            Load16U => MATCH_LHU,
            Load32U => MATCH_LWU,
            Load64 => MATCH_LD,
        };
        // Set rd (for loads) or rs2 (for stores) to A0.
        let htinst = if mmio_op.opcode().is_load() {
            htinst_base | ((GprIndex::A0 as u32) << 20)
        } else {
            htinst_base | ((GprIndex::A0 as u32) << 7)
        };
        htinst as u64
    }

    /// Sets up the shared-memory state for reporting an exit due to `cause` back to the host and
    /// deactivates this vCPU. The vCPU is either returned to the `Available` or `PoweredOff`
    /// state, depending on if the exit cause is resumable.
    pub fn exit(mut self, cause: VmExitCause) {
        let shared = &self.vcpu.shared_area;
        shared.as_ref().set_vstimecmp(CSR.vstimecmp.get());

        use VmExitCause::*;
        match cause {
            ResumableEcall(msg) | FatalEcall(msg) | BlockingEcall(msg, _) => {
                shared.update_with_ecall_exit(msg);
            }
            PageFault(exception, page_addr) => {
                shared.update_with_pf_exit(exception, page_addr.into());
            }
            MmioFault(mmio_op, addr) => {
                let exception = if mmio_op.opcode().is_load() {
                    Exception::GuestLoadPageFault
                } else {
                    Exception::GuestStorePageFault
                };
                shared.update_with_pf_exit(exception, addr);

                // The MMIO instruction is transformed as an ordinary load/store to/from A0, so
                // update A0 with the value the vCPU wants to store.
                use MmioOpcode::*;
                let val = match mmio_op.opcode() {
                    Store8 => self.get_gpr(mmio_op.register()) as u8 as u64,
                    Store16 => self.get_gpr(mmio_op.register()) as u16 as u64,
                    Store32 => self.get_gpr(mmio_op.register()) as u32 as u64,
                    Store64 => self.get_gpr(mmio_op.register()),
                    _ => 0,
                };
                shared.as_ref().set_htinst(Self::mmio_op_to_htinst(mmio_op));
                shared.as_ref().set_gpr(GprIndex::A0, val);

                // We'll complete a load instruction the next time this vCPU is run.
                self.arch.pending_mmio_op = Some(mmio_op);
            }
            Wfi(inst) => {
                shared.update_with_vi_exit(inst.raw() as u64);
            }
            UnhandledTrap(scause) => {
                shared.update_with_unhandled_exit(scause);
            }
        };

        if cause.is_fatal() {
            self.status_set.next_status = VmCpuStatus::PoweredOff;
        } else if let BlockingEcall(_, tlb_version) = cause {
            self.status_set.next_status = VmCpuStatus::Blocked(tlb_version);
        }
    }

    /// Delivers the given exception to the vCPU, setting up its register state to handle the trap
    /// the next time it is run.
    pub fn inject_exception(&mut self, exception: Exception, stval: u64) {
        // Update previous privelege level and interrupt state in VSSTATUS.
        let mut vsstatus = LocalRegisterCopy::<u64, sstatus::Register>::new(CSR.vsstatus.get());
        if vsstatus.is_set(sstatus::sie) {
            vsstatus.modify(sstatus::spie.val(1));
        }
        vsstatus.modify(sstatus::sie.val(0));
        let sstatus =
            LocalRegisterCopy::<u64, sstatus::Register>::new(self.arch.regs.guest_regs.sstatus);
        vsstatus.modify(sstatus::spp.val(sstatus.read(sstatus::spp)));
        CSR.vsstatus.set(vsstatus.get());

        CSR.vscause.set(Trap::Exception(exception).to_scause());
        CSR.vstval.set(stval);
        CSR.vsepc.set(self.arch.regs.guest_regs.sepc);

        // Redirect the vCPU to its STVEC on entry.
        self.arch.regs.guest_regs.sepc = CSR.vstvec.get();
    }

    /// Gets one of the vCPU's general purpose registers.
    pub fn get_gpr(&self, gpr: GprIndex) -> u64 {
        self.arch.regs.guest_regs.gprs.reg(gpr)
    }

    /// Sets one of the vCPU's general-purpose registers.
    pub fn set_gpr(&mut self, gpr: GprIndex, value: u64) {
        self.arch.regs.guest_regs.gprs.set_reg(gpr, value);
    }

    /// Increments the current `sepc` CSR value by `value`.
    pub fn inc_sepc(&mut self, value: u64) {
        self.arch.regs.guest_regs.sepc += value;
    }

    /// Increments SEPC and Updates A0/A1 with the result of an SBI call.
    pub fn set_ecall_result(&mut self, result: SbiReturnType) {
        self.inc_sepc(4); // ECALL is always a 4-byte instruction.
        match result {
            SbiReturnType::Legacy(a0) => {
                self.set_gpr(GprIndex::A0, a0);
            }
            SbiReturnType::Standard(ret) => {
                self.set_gpr(GprIndex::A0, ret.error_code as u64);
                self.set_gpr(GprIndex::A1, ret.return_value);
            }
        }
    }

    /// Returns the location of this vCPU's virtualized IMSIC.
    pub fn get_imsic_location(&self) -> Option<ImsicLocation> {
        self.vcpu.get_imsic_location()
    }

    /// Returns this active vCPU's `ActiveVmPages`.
    pub fn active_pages(&self) -> &ActiveVmPages<'pages, T> {
        // Unwrap ok since it's not possible to externally hold a reference to an `ActiveVmCpu` with
        // `self.active_pages` set to `None`. The only instance where `self.active_pages` is `None` is
        // when we're swapped out to run a child vCPU, in which case that child `ActiveVmCpu` holds
        // the exclusive reference to the parent `ActiveVmCpu`.
        self.active_pages.as_ref().unwrap()
    }

    /// Performs any pending TLB maintenance for this VM's address space.
    pub fn sync_tlb(&mut self) {
        // Exit and re-enter so that we pick up the new TLB version.
        self.active_pages = None;
        self.restore_vm_pages();
    }

    /// Returns a mutable reference to this active vCPU's PMU state.
    pub fn pmu(&mut self) -> &mut VmPmuState {
        &mut self.arch.pmu
    }

    /// Adds `id` to the list of injectable external interrupts.
    pub fn allow_ext_interrupt(&self, id: usize) -> Result<()> {
        self.vcpu
            .ext_interrupts()?
            .lock()
            .allow_interrupt(id)
            .map_err(Error::AllowingInterrupt)
    }

    /// Allows injection of all external interrupts.
    pub fn allow_all_ext_interrupts(&self) -> Result<()> {
        let ext_interrupts = self.vcpu.ext_interrupts()?;
        ext_interrupts.lock().allow_all_interrupts();
        Ok(())
    }

    /// Removes `id` from the list of injectable external interrupts.
    pub fn deny_ext_interrupt(&self, id: usize) -> Result<()> {
        self.vcpu
            .ext_interrupts()?
            .lock()
            .deny_interrupt(id)
            .map_err(Error::DenyingInterrupt)
    }

    /// Disables injection of all external interrupts.
    pub fn deny_all_ext_interrupts(&self) -> Result<()> {
        let ext_interrupts = self.vcpu.ext_interrupts()?;
        ext_interrupts.lock().deny_all_interrupts();
        Ok(())
    }

    // Completes any pending MMIO operation for this CPU.
    fn complete_pending_mmio_op(&mut self) {
        // Complete any pending load operations. The host is expected to have written the value
        // to complete the load to A0.
        if let Some(mmio_op) = self.arch.pending_mmio_op {
            let val = self.vcpu.shared_area.as_ref().gpr(GprIndex::A0);
            use MmioOpcode::*;
            // Write the value to the actual destination register.
            match mmio_op.opcode() {
                Load8 => {
                    self.set_gpr(mmio_op.register(), val as i8 as u64);
                }
                Load8U => {
                    self.set_gpr(mmio_op.register(), val as u8 as u64);
                }
                Load16 => {
                    self.set_gpr(mmio_op.register(), val as i16 as u64);
                }
                Load16U => {
                    self.set_gpr(mmio_op.register(), val as u16 as u64);
                }
                Load32 => {
                    self.set_gpr(mmio_op.register(), val as i32 as u64);
                }
                Load32U => {
                    self.set_gpr(mmio_op.register(), val as u32 as u64);
                }
                Load64 => {
                    self.set_gpr(mmio_op.register(), val);
                }
                _ => (),
            };

            self.arch.pending_mmio_op = None;
            self.vcpu.shared_area.as_ref().set_gpr(GprIndex::A0, 0);

            // Advance SEPC past the faulting instruction.
            self.inc_sepc(mmio_op.len() as u64);
        }
    }

    // Saves the VS-level CSRs.
    fn save_vcpu_csrs(&mut self) {
        let vcpu_csrs = &mut self.arch.regs.guest_vcpu_csrs;
        vcpu_csrs.htimedelta = CSR.htimedelta.get();
        vcpu_csrs.vsstatus = CSR.vsstatus.get();
        vcpu_csrs.vsie = CSR.vsie.get();
        vcpu_csrs.vstvec = CSR.vstvec.get();
        vcpu_csrs.vsscratch = CSR.vsscratch.get();
        vcpu_csrs.vsepc = CSR.vsepc.get();
        vcpu_csrs.vscause = CSR.vscause.get();
        vcpu_csrs.vstval = CSR.vstval.get();
        vcpu_csrs.vsatp = CSR.vsatp.get();
        vcpu_csrs.vstimecmp = CSR.vstimecmp.get();
    }

    // Restores the VS-level CSRs.
    fn restore_vcpu_csrs(&mut self) {
        let vcpu_csrs = &self.arch.regs.guest_vcpu_csrs;
        // Safe as these don't take effect until V=1.
        CSR.htimedelta.set(vcpu_csrs.htimedelta);
        CSR.vsstatus.set(vcpu_csrs.vsstatus);
        CSR.vsie.set(vcpu_csrs.vsie);
        CSR.vstvec.set(vcpu_csrs.vstvec);
        CSR.vsscratch.set(vcpu_csrs.vsscratch);
        CSR.vsepc.set(vcpu_csrs.vsepc);
        CSR.vscause.set(vcpu_csrs.vscause);
        CSR.vstval.set(vcpu_csrs.vstval);
        CSR.vsatp.set(vcpu_csrs.vsatp);
        CSR.vstimecmp.set(vcpu_csrs.vstimecmp);
    }

    // Restores the VM's address space.
    fn restore_vm_pages(&mut self) {
        // Get the VMID to use for this VM's address space on this physical CPU.
        let this_cpu = PerCpu::this_cpu();
        let mut vmid_tracker = this_cpu.vmid_tracker_mut();
        // If VMIDs rolled over next_vmid() will do the necessary flush and the previous TLB version
        // doesn't matter.
        let (vmid, tlb_version) = match self.arch.prev_tlb {
            Some(ref prev_tlb) if prev_tlb.vmid.version() == vmid_tracker.current_version() => {
                (prev_tlb.vmid, Some(prev_tlb.tlb_version))
            }
            _ => (vmid_tracker.next_vmid(), None),
        };

        // enter_with_vmid() will fence if prev_tlb_version is stale.
        let active_pages = self.vm_pages.enter_with_vmid(vmid, tlb_version);

        // Record the TLB version we're using.
        self.arch.prev_tlb = Some(PrevTlb {
            cpu: this_cpu.cpu_id(),
            vmid,
            tlb_version: active_pages.tlb_version(),
        });

        self.active_pages = Some(active_pages);
    }
}

// Interface for vCPU state save/restore on context switch.
trait VmCpuSaveState {
    // Saves this `ActiveVmCpu`'s state, effectively de-activating it until a corresponding call to
    // `restore()`.
    fn save(&mut self);

    // Re-activates this `ActiveVmCpu` by restoring the state that was saved with `save()`.
    fn restore(&mut self);
}

impl<T: GuestStagePagingMode> VmCpuSaveState for ActiveVmCpu<'_, '_, '_, T> {
    fn save(&mut self) {
        self.active_pages = None;
        self.save_vcpu_csrs();
        self.pmu().save_counters();
    }

    fn restore(&mut self) {
        self.restore_vcpu_csrs();
        self.restore_vm_pages();
        self.pmu().restore_counters();
    }
}

impl<T: GuestStagePagingMode> Drop for ActiveVmCpu<'_, '_, '_, T> {
    fn drop(&mut self) {
        // Context-switch back to the parent vCPU.
        self.save();
        if let Some(ref mut p) = self.parent_vcpu {
            p.restore();
        }
    }
}

// ActiveVmCpu is used to guard CSR and TLB state which is local to the current physical CPU and
// thus cannot be safely shared between threads.
impl<T: GuestStagePagingMode> !Sync for ActiveVmCpu<'_, '_, '_, T> {}

/// The availability of vCPU in a VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmCpuStatus {
    /// The vCPU is not powered on.
    PoweredOff,
    /// The vCPU is available to be run.
    Runnable,
    /// The vCPU has been claimed exclusively for running on a (physical) CPU.
    Running,
    /// The vCPU is blocked from running until the VM reaches the given TLB version.
    Blocked(TlbVersion),
}

/// Represents a single virtual CPU of a VM.
pub struct VmCpu {
    // Locking: status -> arch -> ext_interrupts.
    status: RwLock<VmCpuStatus>,
    arch: Mutex<VmCpuArchState>,
    ext_interrupts: Once<Mutex<VmCpuExtInterrupts>>,
    shared_area: VmCpuSharedArea,
    guest_id: PageOwnerId,
    vcpu_id: u64,
}

impl VmCpu {
    /// Creates a new `VmCpu` with ID `vcpu_id` using `shared_area` for the vCPU's shared memory
    /// state area. The returned `VmCpu` is initially powered off.
    pub fn new(vcpu_id: u64, guest_id: PageOwnerId, shared_area: VmCpuSharedArea) -> VmCpu {
        VmCpu {
            status: RwLock::new(VmCpuStatus::PoweredOff),
            arch: Mutex::new(VmCpuArchState::new(guest_id)),
            ext_interrupts: Once::new(),
            shared_area,
            guest_id,
            vcpu_id,
        }
    }

    /// Powers on this vCPU and sets its entry point to the specified SEPC and A1 values.
    pub fn power_on(&self, sepc: u64, opaque: u64) -> Result<()> {
        let mut status = self.status.write();
        if *status != VmCpuStatus::PoweredOff {
            return Err(Error::VmCpuAlreadyPowered);
        }
        let mut arch = self.arch.lock();
        arch.regs.guest_regs.sepc = sepc;
        arch.regs
            .guest_regs
            .gprs
            .set_reg(GprIndex::A0, self.vcpu_id);
        arch.regs.guest_regs.gprs.set_reg(GprIndex::A1, opaque);
        *status = VmCpuStatus::Runnable;
        Ok(())
    }

    /// Powers on this vCPU, latching the entry point from its shared memory state area. Returns
    /// the (SEPC, A1) pair on success.
    pub fn power_on_and_latch_entry_args(&self) -> Result<(u64, u64)> {
        let shared = self.shared_area.as_ref();
        let sepc = shared.sepc();
        let arg = shared.gpr(GprIndex::A1);
        self.power_on(sepc, arg)?;
        Ok((sepc, arg))
    }

    /// Returns the ID of the vCPU in the guest.
    pub fn vcpu_id(&self) -> u64 {
        self.vcpu_id
    }

    /// Returns the runnability status of this vCPU.
    pub fn status(&self) -> VmCpuStatus {
        *self.status.read()
    }

    /// Activates this vCPU, swapping in its register state. Takes exclusive ownership of the
    /// ability to run this vCPU. If `parent_vcpu` is not `None`, its state is saved before this
    /// vCPU is activated and is restored when the returned `ActiveVmCpu` is dropped.
    pub fn activate<'vcpu, 'pages, 'prev: 'vcpu + 'pages, T: GuestStagePagingMode>(
        &'vcpu self,
        vm_pages: FinalizedVmPages<'pages, T>,
        parent_vcpu: Option<&'prev mut ActiveVmCpu<T>>,
    ) -> Result<ActiveVmCpu<'vcpu, 'pages, 'prev, T>> {
        let mut status = self.status.write();
        use VmCpuStatus::*;
        match *status {
            Blocked(tlb_version) if tlb_version > vm_pages.min_tlb_version() => {
                Err(Error::VmCpuBlocked)
            }
            Runnable | Blocked(_) => {
                if self.guest_id != vm_pages.page_owner_id() {
                    return Err(Error::WrongAddressSpace);
                }

                // We must be bound to the current CPU if IMSIC virtualization is enabled.
                if let Some(ext_interrupts) = self.ext_interrupts.get() &&
                    !ext_interrupts.lock().is_bound_on_this_cpu()
                {
                    return Err(Error::VmCpuNotBound);
                }

                let active_vcpu = ActiveVmCpu::restore_from(self, vm_pages, parent_vcpu)?;
                *status = Running;
                Ok(active_vcpu)
            }
            Running => Err(Error::VmCpuRunning),
            PoweredOff => Err(Error::VmCpuOff),
        }
    }

    /// Enables IMSIC virtualization for this vCPU, setting the location of the virtualized ISMIC
    /// in guest physical address space.
    pub fn enable_imsic_virtualization(&self, imsic_location: ImsicLocation) -> Result<()> {
        self.ext_interrupts
            .call_once(|| Mutex::new(VmCpuExtInterrupts::new(imsic_location)));
        if self.ext_interrupts.get().unwrap().lock().imsic_location() != imsic_location {
            return Err(Error::ImsicLocationAlreadySet);
        }
        Ok(())
    }

    fn ext_interrupts(&self) -> Result<&Mutex<VmCpuExtInterrupts>> {
        self.ext_interrupts
            .get()
            .ok_or(Error::NoImsicVirtualization)
    }

    /// Returns the location of this vCPU's virtualized IMSIC.
    pub fn get_imsic_location(&self) -> Option<ImsicLocation> {
        self.ext_interrupts()
            .ok()
            .map(|ei| ei.lock().imsic_location())
    }

    /// Prepares to bind this vCPU to `interrupt_file` on the current physical CPU.
    pub fn bind_imsic_prepare(&self, interrupt_file: ImsicFileId) -> Result<()> {
        // We skip the self.status check here (and similarly for the other bind/unbind calls)
        // since the only way for the vCPU to be presently running is to be bound to a different
        // physical CPU.
        self.ext_interrupts()?
            .lock()
            .bind_imsic_prepare(interrupt_file)
            .map_err(Error::Binding)
    }

    /// Completes the IMSIC bind operation started in `bind_imsic_prepare()`.
    pub fn bind_imsic_finish(&self) -> Result<()> {
        let interrupt_file = self
            .ext_interrupts()?
            .lock()
            .bind_imsic_finish()
            .map_err(Error::Binding)?;

        // Update VGEIN so that the selected interrupt file gets used next time the vCPU is run.
        let mut arch = self.arch.lock();
        let mut hstatus =
            LocalRegisterCopy::<u64, hstatus::Register>::new(arch.regs.guest_regs.hstatus);
        hstatus.modify(hstatus::vgein.val(interrupt_file.bits() as u64));
        arch.regs.guest_regs.hstatus = hstatus.get();
        Ok(())
    }

    /// Prepares to rebind this vCPU to `interrupt_file` on the current physical CPU.
    pub fn rebind_imsic_prepare(&self, interrupt_file: ImsicFileId) -> Result<()> {
        self.ext_interrupts()?
            .lock()
            .rebind_imsic_prepare(interrupt_file)
            .map_err(Error::Rebinding)?;
        Ok(())
    }

    /// Copies the guest interrupt file state on the previous CPU.
    pub fn rebind_imsic_clone(&self) -> Result<()> {
        self.ext_interrupts()?
            .lock()
            .rebind_imsic_clone()
            .map_err(Error::Rebinding)?;
        Ok(())
    }

    /// Get the previous guest interrupt file's location.
    pub fn prev_imsic_location(&self) -> Result<ImsicLocation> {
        let prev_imsic_loc = self
            .ext_interrupts()?
            .lock()
            .prev_imsic_location()
            .map_err(Error::Rebinding)?;
        Ok(prev_imsic_loc)
    }

    /// Completes the IMSIC rebind operation started in `rebind_imsic_prepare()`.
    pub fn rebind_imsic_finish(&self) -> Result<()> {
        let interrupt_file = self
            .ext_interrupts()?
            .lock()
            .rebind_imsic_finish()
            .map_err(Error::Rebinding)?;

        // Update VGEIN so that the selected interrupt file gets used next time the vCPU is run.
        let mut arch = self.arch.lock();
        let mut hstatus =
            LocalRegisterCopy::<u64, hstatus::Register>::new(arch.regs.guest_regs.hstatus);
        hstatus.modify(hstatus::vgein.val(interrupt_file.bits() as u64));
        arch.regs.guest_regs.hstatus = hstatus.get();
        Ok(())
    }

    /// Prepares to unbind this vCPU from its current interrupt file.
    pub fn unbind_imsic_prepare(&self) -> Result<()> {
        self.ext_interrupts()?
            .lock()
            .unbind_imsic_prepare()
            .map_err(Error::Unbinding)
    }

    /// Completes the IMSIC unbind operation started in `unbind_imsic_prepare()`.
    pub fn unbind_imsic_finish(&self) -> Result<()> {
        self.ext_interrupts()?
            .lock()
            .unbind_imsic_finish()
            .map_err(Error::Unbinding)
    }

    /// Injects the specified external interrupt ID into this vCPU, if allowed.
    pub fn inject_ext_interrupt(&self, id: usize) -> Result<()> {
        self.ext_interrupts()?
            .lock()
            .inject_interrupt(id)
            .map_err(Error::InjectingInterrupt)
    }
}

/// The set of vCPUs in a VM.
pub struct VmCpus {
    inner: [Once<PageBox<VmCpu>>; VM_CPUS_MAX],
}

impl VmCpus {
    /// Creates a new vCPU tracking structure.
    pub fn new() -> Self {
        Self {
            inner: [(); VM_CPUS_MAX].map(|_| Once::new()),
        }
    }

    /// Adds the given vCPU to the set of vCPUs.
    pub fn add_vcpu(&self, vcpu: PageBox<VmCpu>) -> Result<()> {
        let vcpu_id = vcpu.vcpu_id();
        let once_entry = self.inner.get(vcpu_id as usize).ok_or(Error::BadVmCpuId)?;

        // CAVEAT: following code is tricky.
        //
        // What we need is a OnceLock or a way to check that we have
        // indeed set the value we passed. Since we don't have this
        // (for now), we determine if the `vcpu` has been set by
        // checking the underlying pointer of the `PageBox`.
        let ptr: *const VmCpu = &*vcpu;
        let once_val = once_entry.call_once(|| vcpu);
        let once_ptr: *const VmCpu = &**once_val;

        if once_ptr != ptr {
            Err(Error::VmCpuExists)
        } else {
            Ok(())
        }
    }

    /// Returns a reference to the vCPU with `vcpu_id` if it exists.
    pub fn get_vcpu(&self, vcpu_id: u64) -> Result<&VmCpu> {
        let vcpu = self
            .inner
            .get(vcpu_id as usize)
            .and_then(|once| once.get())
            .ok_or(Error::VmCpuNotFound)?;
        Ok(vcpu)
    }

    /// Returns the number of pages that must be donated to create a vCPU.
    pub const fn required_state_pages_per_vcpu() -> u64 {
        PageBox::<VmCpu>::required_pages()
    }
}

// Safety: Each VmCpu is wrapped with a Mutex to provide safe concurrent access to VmCpu and its
// composite structures from within the hypervisor, and the VmCpu API provides safe accessors to
// the shared-memory vCPU state structure.
unsafe impl Sync for VmCpus {}
unsafe impl Send for VmCpus {}
