// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Copyright © 2020, Microsoft Corporation
//

#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_macros)]
#![allow(non_upper_case_globals)]

use crate::arch::emulator::{EmulationError, PlatformEmulator, PlatformError};
#[cfg(target_arch = "x86_64")]
use crate::arch::x86::emulator::{Emulator, EmulatorCpuState};
use crate::cpu;
use crate::cpu::Vcpu;
use crate::hypervisor;
use crate::vm::{self, VmmOps};
pub use mshv_bindings::*;
use mshv_ioctls::{set_registers_64, Mshv, VcpuFd, VmFd};

use serde_derive::{Deserialize, Serialize};
use std::sync::Arc;
use vm::DataMatch;
// x86_64 dependencies
#[cfg(target_arch = "x86_64")]
pub mod x86_64;
use std::convert::TryInto;

use vmm_sys_util::eventfd::EventFd;
#[cfg(target_arch = "x86_64")]
pub use x86_64::VcpuMshvState as CpuState;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

use crate::device;

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::{Mutex, RwLock};
use std::thread;

pub const PAGE_SHIFT: usize = 12;

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
pub struct HvState {
    hypercall_page: u64,
}

pub use HvState as VmState;

struct IrqfdCtrlEpollHandler {
    vm: Arc<dyn vm::Vm>, /* For issuing hypercall */
    irqfd: EventFd,      /* Registered by caller */
    kill: EventFd,       /* Created by us, signal thread exit */
    epoll_fd: RawFd,     /* epoll fd */
    gsi: u32,
    gsi_routes: Arc<RwLock<HashMap<u32, MshvIrqRoutingEntry>>>,
}

fn register_listener(
    epoll_fd: RawFd,
    fd: RawFd,
    ev_type: epoll::Events,
    data: u64,
) -> std::result::Result<(), io::Error> {
    epoll::ctl(
        epoll_fd,
        epoll::ControlOptions::EPOLL_CTL_ADD,
        fd,
        epoll::Event::new(ev_type, data),
    )
}

const KILL_EVENT: u16 = 1;
const IRQFD_EVENT: u16 = 2;

impl IrqfdCtrlEpollHandler {
    fn run_ctrl(&mut self) {
        self.epoll_fd = epoll::create(true).unwrap();
        let epoll_file = unsafe { File::from_raw_fd(self.epoll_fd) };

        register_listener(
            epoll_file.as_raw_fd(),
            self.kill.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(KILL_EVENT),
        )
        .unwrap_or_else(|err| {
            info!(
                "IrqfdCtrlEpollHandler: failed to register listener: {:?}",
                err
            );
        });

        register_listener(
            epoll_file.as_raw_fd(),
            self.irqfd.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(IRQFD_EVENT),
        )
        .unwrap_or_else(|err| {
            info!(
                "IrqfdCtrlEpollHandler: failed to register listener: {:?}",
                err
            );
        });

        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); 2];

        'epoll: loop {
            let num_events = match epoll::wait(epoll_file.as_raw_fd(), -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    panic!("irqfd epoll ???");
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    KILL_EVENT => {
                        break 'epoll;
                    }
                    IRQFD_EVENT => {
                        debug!("IRQFD_EVENT received, inject to guest");
                        let _ = self.irqfd.read().unwrap();
                        let gsi_routes = self.gsi_routes.read().unwrap();

                        if let Some(e) = gsi_routes.get(&self.gsi) {
                            assert_virtual_interrupt(&self.vm, &e);
                        } else {
                            debug!("No routing info found for GSI {}", self.gsi);
                        }
                    }
                    _ => {
                        error!("Unknown event");
                    }
                }
            }
        }
    }
}

// Translate from architectural defined delivery mode to Hyper-V type
// See Intel SDM vol3 10.11.2
fn get_interrupt_type(delivery_mode: u8) -> Option<hv_interrupt_type> {
    match delivery_mode {
        0 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_FIXED),
        1 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_LOWESTPRIORITY),
        2 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_SMI),
        4 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_NMI),
        5 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_INIT),
        7 => Some(hv_interrupt_type_HV_X64_INTERRUPT_TYPE_EXTINT),
        _ => None,
    }
}

// See Intel SDM vol3 10.11.1
// We assume APIC ID and Hyper-V Vcpu ID are the same value
// This holds true for HvLite
fn get_destination(message_address: u32) -> u64 {
    ((message_address >> 12) & 0xff).into()
}

fn get_destination_mode(message_address: u32) -> bool {
    if (message_address >> 2) & 0x1 == 0x1 {
        return true;
    }

    false
}

fn get_redirection_hint(message_address: u32) -> bool {
    if (message_address >> 3) & 0x1 == 0x1 {
        return true;
    }

    false
}

fn get_vector(message_data: u32) -> u8 {
    (message_data & 0xff) as u8
}

// True means level triggered
fn get_trigger_mode(message_data: u32) -> bool {
    if (message_data >> 15) & 0x1 == 0x1 {
        return true;
    }

    false
}

fn get_delivery_mode(message_data: u32) -> u8 {
    ((message_data & 0x700) >> 8) as u8
}

// Only meaningful with level triggered interrupts
// True => High active
// False => Low active
fn get_level(message_data: u32) -> bool {
    if (message_data >> 14) & 0x1 == 0x1 {
        return true;
    }

    false
}

fn assert_virtual_interrupt(vm: &Arc<dyn vm::Vm>, e: &MshvIrqRoutingEntry) {
    // GSI routing contains MSI information.
    // We still need to translate that to APIC ID etc

    debug!("Inject {:x?}", e);

    let MshvIrqRouting::Msi(msi) = e.route;

    /* Make an assumption here ... */
    if msi.address_hi != 0 {
        panic!("MSI high address part is not zero");
    }

    let typ = get_interrupt_type(get_delivery_mode(msi.data)).unwrap();
    let apic_id = get_destination(msi.address_lo);
    let vector = get_vector(msi.data);
    let level_triggered = get_trigger_mode(msi.data);
    let logical_destination_mode = get_destination_mode(msi.address_lo);

    debug!(
        "{:x} {:x} {:x} {} {}",
        typ, apic_id, vector, level_triggered, logical_destination_mode
    );

    vm.request_virtual_interrupt(
        typ as u8,
        apic_id,
        vector.into(),
        level_triggered,
        logical_destination_mode,
        false,
    )
    .unwrap_or_else(|err| {
        info!("Failed to request virtual interrupt: {:?}", err);
    });
}

/// Wrapper over mshv system ioctls.
pub struct MshvHypervisor {
    mshv: Mshv,
}

impl MshvHypervisor {
    /// Create a hypervisor based on Mshv
    pub fn new() -> hypervisor::Result<MshvHypervisor> {
        let mshv_obj =
            Mshv::new().map_err(|e| hypervisor::HypervisorError::HypervisorCreate(e.into()))?;
        Ok(MshvHypervisor { mshv: mshv_obj })
    }
}
/// Implementation of Hypervisor trait for Mshv
/// Example:
/// #[cfg(feature = "mshv")]
/// extern crate hypervisor
/// let mshv = hypervisor::mshv::MshvHypervisor::new().unwrap();
/// let hypervisor: Arc<dyn hypervisor::Hypervisor> = Arc::new(mshv);
/// let vm = hypervisor.create_vm().expect("new VM fd creation failed");
///
impl hypervisor::Hypervisor for MshvHypervisor {
    /// Create a mshv vm object and return the object as Vm trait object
    /// Example
    /// # extern crate hypervisor;
    /// # use hypervisor::MshvHypervisor;
    /// use hypervisor::MshvVm;
    /// let hypervisor = MshvHypervisor::new().unwrap();
    /// let vm = hypervisor.create_vm().unwrap()
    ///
    fn create_vm(&self) -> hypervisor::Result<Arc<dyn vm::Vm>> {
        let fd: VmFd;
        loop {
            match self.mshv.create_vm() {
                Ok(res) => fd = res,
                Err(e) => {
                    if e.errno() == libc::EINTR {
                        // If the error returned is EINTR, which means the
                        // ioctl has been interrupted, we have to retry as
                        // this can't be considered as a regular error.
                        continue;
                    } else {
                        return Err(hypervisor::HypervisorError::VmCreate(e.into()));
                    }
                }
            }
            break;
        }

        let msr_list = self.get_msr_list()?;
        let num_msrs = msr_list.as_fam_struct_ref().nmsrs as usize;
        let mut msrs = MsrEntries::new(num_msrs);
        let indices = msr_list.as_slice();
        let msr_entries = msrs.as_mut_slice();
        for (pos, index) in indices.iter().enumerate() {
            msr_entries[pos].index = *index;
        }
        let vm_fd = Arc::new(fd);

        let irqfds = Mutex::new(HashMap::new());
        let ioeventfds = Arc::new(RwLock::new(HashMap::new()));
        let gsi_routes = Arc::new(RwLock::new(HashMap::new()));

        Ok(Arc::new(MshvVm {
            fd: vm_fd,
            msrs,
            irqfds,
            ioeventfds,
            gsi_routes,
            hv_state: hv_state_init(),
            vmmops: None,
        }))
    }
    ///
    /// Get the supported CpuID
    ///
    fn get_cpuid(&self) -> hypervisor::Result<CpuId> {
        Ok(CpuId::new(1 as usize))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Retrieve the list of MSRs supported by KVM.
    ///
    fn get_msr_list(&self) -> hypervisor::Result<MsrList> {
        self.mshv
            .get_msr_index_list()
            .map_err(|e| hypervisor::HypervisorError::GetMsrList(e.into()))
    }
}

#[derive(Clone)]
// A software emulated TLB.
// This is mostly used by the instruction emulator to cache gva to gpa translations
// passed from the hypervisor.
struct SoftTLB {
    addr_map: HashMap<u64, u64>,
}

impl SoftTLB {
    fn new() -> SoftTLB {
        SoftTLB {
            addr_map: HashMap::new(),
        }
    }

    // Adds a gva -> gpa mapping into the TLB.
    fn add_mapping(&mut self, gva: u64, gpa: u64) -> Result<(), PlatformError> {
        *self.addr_map.entry(gva).or_insert(gpa) = gpa;
        Ok(())
    }

    // Do the actual gva -> gpa translation
    fn translate(&self, gva: u64) -> Result<u64, PlatformError> {
        self.addr_map
            .get(&gva)
            .ok_or_else(|| PlatformError::UnmappedGVA(anyhow!("{:#?}", gva)))
            .map(|v| *v)

        // TODO Check if we could fallback to e.g. an hypercall for doing
        // the translation for us.
    }

    // FLush the TLB, all mappings are removed.
    fn flush(&mut self) -> Result<(), PlatformError> {
        self.addr_map.clear();

        Ok(())
    }
}

#[allow(clippy::type_complexity)]
/// Vcpu struct for Hyper-V
pub struct MshvVcpu {
    fd: VcpuFd,
    vp_index: u8,
    cpuid: CpuId,
    msrs: MsrEntries,
    ioeventfds: Arc<RwLock<HashMap<IoEventAddress, (Option<DataMatch>, EventFd)>>>,
    gsi_routes: Arc<RwLock<HashMap<u32, MshvIrqRoutingEntry>>>,
    hv_state: Arc<RwLock<HvState>>, // Mshv State
    vmmops: Option<Arc<Box<dyn vm::VmmOps>>>,
}

/// Implementation of Vcpu trait for Microsoft Hyper-V
/// Example:
/// #[cfg(feature = "mshv")]
/// extern crate hypervisor
/// let mshv = hypervisor::mshv::MshvHypervisor::new().unwrap();
/// let hypervisor: Arc<dyn hypervisor::Hypervisor> = Arc::new(mshv);
/// let vm = hypervisor.create_vm().expect("new VM fd creation failed");
/// let vcpu = vm.create_vcpu(0).unwrap();
/// vcpu.get/set().unwrap()
///
impl cpu::Vcpu for MshvVcpu {
    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns the vCPU general purpose registers.
    ///
    fn get_regs(&self) -> cpu::Result<StandardRegisters> {
        self.fd
            .get_regs()
            .map_err(|e| cpu::HypervisorCpuError::GetStandardRegs(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Sets the vCPU general purpose registers.
    ///
    fn set_regs(&self, regs: &StandardRegisters) -> cpu::Result<()> {
        self.fd
            .set_regs(regs)
            .map_err(|e| cpu::HypervisorCpuError::SetStandardRegs(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns the vCPU special registers.
    ///
    fn get_sregs(&self) -> cpu::Result<SpecialRegisters> {
        self.fd
            .get_sregs()
            .map_err(|e| cpu::HypervisorCpuError::GetSpecialRegs(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Sets the vCPU special registers.
    ///
    fn set_sregs(&self, sregs: &SpecialRegisters) -> cpu::Result<()> {
        self.fd
            .set_sregs(sregs)
            .map_err(|e| cpu::HypervisorCpuError::SetSpecialRegs(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns the floating point state (FPU) from the vCPU.
    ///
    fn get_fpu(&self) -> cpu::Result<FpuState> {
        self.fd
            .get_fpu()
            .map_err(|e| cpu::HypervisorCpuError::GetFloatingPointRegs(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Set the floating point state (FPU) of a vCPU.
    ///
    fn set_fpu(&self, fpu: &FpuState) -> cpu::Result<()> {
        self.fd
            .set_fpu(fpu)
            .map_err(|e| cpu::HypervisorCpuError::SetFloatingPointRegs(e.into()))
    }

    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns the model-specific registers (MSR) for this vCPU.
    ///
    fn get_msrs(&self, msrs: &mut MsrEntries) -> cpu::Result<usize> {
        self.fd
            .get_msrs(msrs)
            .map_err(|e| cpu::HypervisorCpuError::GetMsrEntries(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Setup the model-specific registers (MSR) for this vCPU.
    /// Returns the number of MSR entries actually written.
    ///
    fn set_msrs(&self, msrs: &MsrEntries) -> cpu::Result<usize> {
        self.fd
            .set_msrs(msrs)
            .map_err(|e| cpu::HypervisorCpuError::SetMsrEntries(e.into()))
    }

    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call that returns the vcpu's current "xcrs".
    ///
    fn get_xcrs(&self) -> cpu::Result<ExtendedControlRegisters> {
        self.fd
            .get_xcrs()
            .map_err(|e| cpu::HypervisorCpuError::GetXcsr(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call that sets the vcpu's current "xcrs".
    ///
    fn set_xcrs(&self, xcrs: &ExtendedControlRegisters) -> cpu::Result<()> {
        self.fd
            .set_xcrs(&xcrs)
            .map_err(|e| cpu::HypervisorCpuError::SetXcsr(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns currently pending exceptions, interrupts, and NMIs as well as related
    /// states of the vcpu.
    ///
    fn get_vcpu_events(&self) -> cpu::Result<VcpuEvents> {
        self.fd
            .get_vcpu_events()
            .map_err(|e| cpu::HypervisorCpuError::GetVcpuEvents(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Sets pending exceptions, interrupts, and NMIs as well as related states
    /// of the vcpu.
    ///
    fn set_vcpu_events(&self, events: &VcpuEvents) -> cpu::Result<()> {
        self.fd
            .set_vcpu_events(events)
            .map_err(|e| cpu::HypervisorCpuError::SetVcpuEvents(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call to enable HyperV SynIC
    ///
    fn enable_hyperv_synic(&self) -> cpu::Result<()> {
        /* We always have SynIC enabled on MSHV */
        Ok(())
    }
    fn run(&self) -> std::result::Result<cpu::VmExit, cpu::HypervisorCpuError> {
        Ok(cpu::VmExit::Ignore)
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call to setup the CPUID registers.
    ///
    fn set_cpuid2(&self, cpuid: &CpuId) -> cpu::Result<()> {
        Ok(())
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call to retrieve the CPUID registers.
    ///
    fn get_cpuid2(&self, num_entries: usize) -> cpu::Result<CpuId> {
        Ok(self.cpuid.clone())
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Returns the state of the LAPIC (Local Advanced Programmable Interrupt Controller).
    ///
    fn get_lapic(&self) -> cpu::Result<LapicState> {
        self.fd
            .get_lapic()
            .map_err(|e| cpu::HypervisorCpuError::GetlapicState(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Sets the state of the LAPIC (Local Advanced Programmable Interrupt Controller).
    ///
    fn set_lapic(&self, lapic: &LapicState) -> cpu::Result<()> {
        self.fd
            .set_lapic(lapic)
            .map_err(|e| cpu::HypervisorCpuError::SetLapicState(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call that returns the vcpu's current "xsave struct".
    ///
    fn get_xsave(&self) -> cpu::Result<Xsave> {
        self.fd
            .get_xsave()
            .map_err(|e| cpu::HypervisorCpuError::GetXsaveState(e.into()))
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// X86 specific call that sets the vcpu's current "xsave struct".
    ///
    fn set_xsave(&self, xsave: &Xsave) -> cpu::Result<()> {
        self.fd
            .set_xsave(*xsave)
            .map_err(|e| cpu::HypervisorCpuError::SetXsaveState(e.into()))
    }
    fn set_state(&self, state: &CpuState) -> cpu::Result<()> {
        Ok(())
    }
    fn state(&self) -> cpu::Result<CpuState> {
        unimplemented!();
    }
}

struct MshvEmulatorContext<'a> {
    vcpu: &'a MshvVcpu,
    tlb: SoftTLB,
}

/// Platform emulation for Hyper-V
impl<'a> PlatformEmulator for MshvEmulatorContext<'a> {
    type CpuState = EmulatorCpuState;

    fn read_memory(&self, gva: u64, data: &mut [u8]) -> Result<(), PlatformError> {
        let gpa = self.tlb.translate(gva)?;
        debug!(
            "mshv emulator: memory read {} bytes from [{:#x} -> {:#x}]",
            data.len(),
            gva,
            gpa
        );

        if let Some(vmmops) = &self.vcpu.vmmops {
            vmmops
                .mmio_read(gpa, data)
                .map_err(|e| PlatformError::MemoryReadFailure(e.into()))?;
        }

        Ok(())
    }

    fn write_memory(&mut self, gva: u64, data: &[u8]) -> Result<(), PlatformError> {
        let gpa = self.tlb.translate(gva)?;
        debug!(
            "mshv emulator: memory write {} bytes at [{:#x} -> {:#x}]",
            data.len(),
            gva,
            gpa
        );

        if let Some((datamatch, efd)) = self
            .vcpu
            .ioeventfds
            .read()
            .unwrap()
            .get(&IoEventAddress::Mmio(gpa))
        {
            debug!("ioevent {:x} {:x?} {}", gpa, datamatch, efd.as_raw_fd());

            /* TODO: use datamatch to provide the correct semantics */
            efd.write(1).unwrap();
        }

        if let Some(vmmops) = &self.vcpu.vmmops {
            vmmops
                .mmio_write(gpa, data)
                .map_err(|e| PlatformError::MemoryWriteFailure(e.into()))?;
        }

        Ok(())
    }

    fn cpu_state(&self, cpu_id: usize) -> Result<Self::CpuState, PlatformError> {
        if cpu_id != self.vcpu.vp_index as usize {
            return Err(PlatformError::GetCpuStateFailure(anyhow!(
                "CPU id mismatch {:?} {:?}",
                cpu_id,
                self.vcpu.vp_index
            )));
        }

        let regs = self
            .vcpu
            .get_regs()
            .map_err(|e| PlatformError::GetCpuStateFailure(e.into()))?;
        let sregs = self
            .vcpu
            .get_sregs()
            .map_err(|e| PlatformError::GetCpuStateFailure(e.into()))?;

        debug!("mshv emulator: Getting new CPU state");
        debug!("mshv emulator: {:#x?}", regs);

        Ok(EmulatorCpuState { regs, sregs })
    }

    fn set_cpu_state(&self, cpu_id: usize, state: Self::CpuState) -> Result<(), PlatformError> {
        if cpu_id != self.vcpu.vp_index as usize {
            return Err(PlatformError::SetCpuStateFailure(anyhow!(
                "CPU id mismatch {:?} {:?}",
                cpu_id,
                self.vcpu.vp_index
            )));
        }

        debug!("mshv emulator: Setting new CPU state");
        debug!("mshv emulator: {:#x?}", state.regs);

        self.vcpu
            .set_regs(&state.regs)
            .map_err(|e| PlatformError::SetCpuStateFailure(e.into()))?;
        self.vcpu
            .set_sregs(&state.sregs)
            .map_err(|e| PlatformError::SetCpuStateFailure(e.into()))
    }

    fn gva_to_gpa(&self, gva: u64) -> Result<u64, PlatformError> {
        self.tlb.translate(gva)
    }

    fn fetch(&self, ip: u64, instruction_bytes: &mut [u8]) -> Result<(), PlatformError> {
        Err(PlatformError::MemoryReadFailure(anyhow!("unimplemented")))
    }
}

#[allow(clippy::type_complexity)]
/// Wrapper over Mshv VM ioctls.
pub struct MshvVm {
    fd: Arc<VmFd>,
    msrs: MsrEntries,
    // Emulate irqfd
    irqfds: Mutex<HashMap<u32, (EventFd, EventFd)>>,
    // Emulate ioeventfd
    ioeventfds: Arc<RwLock<HashMap<IoEventAddress, (Option<DataMatch>, EventFd)>>>,
    // GSI routing information
    gsi_routes: Arc<RwLock<HashMap<u32, MshvIrqRoutingEntry>>>,
    // Hypervisor State
    hv_state: Arc<RwLock<HvState>>,
    vmmops: Option<Arc<Box<dyn vm::VmmOps>>>,
}

fn hv_state_init() -> Arc<RwLock<HvState>> {
    Arc::new(RwLock::new(HvState { hypercall_page: 0 }))
}

///
/// Implementation of Vm trait for Mshv
/// Example:
/// #[cfg(feature = "mshv")]
/// # extern crate hypervisor;
/// # use hypervisor::MshvHypervisor;
/// let mshv = MshvHypervisor::new().unwrap();
/// let hypervisor: Arc<dyn hypervisor::Hypervisor> = Arc::new(mshv);
/// let vm = hypervisor.create_vm().expect("new VM fd creation failed");
/// vm.set/get().unwrap()
///
impl vm::Vm for MshvVm {
    #[cfg(target_arch = "x86_64")]
    ///
    /// Sets the address of the three-page region in the VM's address space.
    ///
    fn set_tss_address(&self, offset: usize) -> vm::Result<()> {
        Ok(())
    }
    ///
    /// Creates an in-kernel interrupt controller.
    ///
    fn create_irq_chip(&self) -> vm::Result<()> {
        Ok(())
    }
    ///
    /// Registers an event that will, when signaled, trigger the `gsi` IRQ.
    ///
    fn register_irqfd(&self, fd: &EventFd, gsi: u32, vm: Arc<dyn vm::Vm>) -> vm::Result<()> {
        let dup_fd = fd.try_clone().unwrap();
        let kill_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();

        let mut ctrl_handler = IrqfdCtrlEpollHandler {
            vm,
            kill: kill_fd.try_clone().unwrap(),
            irqfd: fd.try_clone().unwrap(),
            epoll_fd: 0,
            gsi,
            gsi_routes: self.gsi_routes.clone(),
        };

        debug!("register_irqfd fd {} gsi {}", fd.as_raw_fd(), gsi);

        thread::Builder::new()
            .name(format!("irqfd_{}", gsi))
            .spawn(move || ctrl_handler.run_ctrl())
            .unwrap();

        self.irqfds.lock().unwrap().insert(gsi, (dup_fd, kill_fd));

        Ok(())
    }
    ///
    /// Unregisters an event that will, when signaled, trigger the `gsi` IRQ.
    ///
    fn unregister_irqfd(&self, _fd: &EventFd, gsi: u32) -> vm::Result<()> {
        debug!("unregister_irqfd fd {} gsi {}", _fd.as_raw_fd(), gsi);
        let (_, kill_fd) = self.irqfds.lock().unwrap().remove(&gsi).unwrap();
        kill_fd.write(1).unwrap();
        Ok(())
    }
    ///
    /// Creates a VcpuFd object from a vcpu RawFd.
    ///
    fn create_vcpu(
        &self,
        id: u8,
        vmmops: Option<Arc<Box<dyn VmmOps>>>,
    ) -> vm::Result<Arc<dyn cpu::Vcpu>> {
        let vc = self
            .fd
            .create_vcpu(id)
            .map_err(|e| vm::HypervisorVmError::CreateVcpu(e.into()))?;
        let vcpu = MshvVcpu {
            fd: vc,
            vp_index: id,
            cpuid: CpuId::new(1 as usize),
            msrs: self.msrs.clone(),
            ioeventfds: self.ioeventfds.clone(),
            gsi_routes: self.gsi_routes.clone(),
            hv_state: self.hv_state.clone(),
            vmmops,
        };
        Ok(Arc::new(vcpu))
    }
    #[cfg(target_arch = "x86_64")]
    fn enable_split_irq(&self) -> vm::Result<()> {
        Ok(())
    }
    fn register_ioevent(
        &self,
        fd: &EventFd,
        addr: &IoEventAddress,
        datamatch: Option<DataMatch>,
    ) -> vm::Result<()> {
        let dup_fd = fd.try_clone().unwrap();

        debug!(
            "register_ioevent fd {} addr {:x?} datamatch {:?}",
            fd.as_raw_fd(),
            addr,
            datamatch
        );

        self.ioeventfds
            .write()
            .unwrap()
            .insert(*addr, (datamatch, dup_fd));
        Ok(())
    }
    /// Unregister an event from a certain address it has been previously registered to.
    fn unregister_ioevent(&self, fd: &EventFd, addr: &IoEventAddress) -> vm::Result<()> {
        debug!("unregister_ioevent fd {} addr {:x?}", fd.as_raw_fd(), addr);
        self.ioeventfds.write().unwrap().remove(addr).unwrap();
        Ok(())
    }

    /// Creates/modifies a guest physical memory slot.
    fn set_user_memory_region(&self, user_memory_region: MemoryRegion) -> vm::Result<()> {
        self.fd
            .map_user_memory(user_memory_region)
            .map_err(|e| vm::HypervisorVmError::SetUserMemory(e.into()))?;
        Ok(())
    }

    fn make_user_memory_region(
        &self,
        _slot: u32,
        guest_phys_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
        readonly: bool,
        log_dirty_pages: bool,
    ) -> MemoryRegion {
        let mut flags = HV_MAP_GPA_READABLE | HV_MAP_GPA_EXECUTABLE;
        if !readonly {
            flags |= HV_MAP_GPA_WRITABLE;
        }

        mshv_user_mem_region {
            flags,
            guest_pfn: guest_phys_addr >> PAGE_SHIFT,
            size: memory_size,
            userspace_addr: userspace_addr as u64,
        }
    }

    fn create_passthrough_device(&self) -> vm::Result<Arc<dyn device::Device>> {
        Err(vm::HypervisorVmError::CreatePassthroughDevice(anyhow!(
            "No passthrough support"
        )))
    }

    fn set_gsi_routing(&self, irq_routing: &[IrqRoutingEntry]) -> vm::Result<()> {
        let mut routes = self.gsi_routes.write().unwrap();

        routes.drain();

        for r in irq_routing {
            debug!("gsi routing {:x?}", r);
            routes.insert(r.gsi, *r);
        }

        Ok(())
    }

    fn request_virtual_interrupt(
        &self,
        interrupt_type: u8,
        apic_id: u64,
        vector: u32,
        level_triggered: bool,
        logical_destination_mode: bool,
        long_mode: bool,
    ) -> vm::Result<()> {
        Ok(())
    }
    ///
    /// Get the Vm state. Return VM specific data
    ///
    fn state(&self) -> vm::Result<VmState> {
        Ok(*self.hv_state.read().unwrap())
    }
    ///
    /// Set the VM state
    ///
    fn set_state(&self, state: VmState) -> vm::Result<()> {
        self.hv_state.write().unwrap().hypercall_page = state.hypercall_page;
        Ok(())
    }
    ///
    /// Get dirty pages bitmap (one bit per page)
    ///
    fn get_dirty_log(&self, slot: u32, memory_size: u64) -> vm::Result<Vec<u64>> {
        unimplemented!();
    }
}
pub use hv_cpuid_entry as CpuIdEntry;

#[derive(Copy, Clone, Debug)]
pub struct MshvIrqRoutingMsi {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
}

#[derive(Copy, Clone, Debug)]
pub enum MshvIrqRouting {
    Msi(MshvIrqRoutingMsi),
}

#[derive(Copy, Clone, Debug)]
pub struct MshvIrqRoutingEntry {
    pub gsi: u32,
    pub route: MshvIrqRouting,
}
pub type IrqRoutingEntry = MshvIrqRoutingEntry;

pub const CPUID_FLAG_VALID_INDEX: u32 = 0;
