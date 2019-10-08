use crate::error::{self, Error, Result};
use crate::memory::GuestPhysAddr;
use crate::registers::{self, Cr4, GdtrBase, IdtrBase};
use crate::vmcs;
use crate::vmx;
use alloc::vec::Vec;
use x86_64::registers::control::{Cr0, Cr3};
use x86_64::registers::model_specific::{Efer, FsBase, GsBase, Msr};
use x86_64::registers::rflags;
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::frame::PhysFrame;
use x86_64::structures::paging::page::Size4KiB;
use x86_64::structures::paging::FrameAllocator;
use x86_64::PhysAddr;

pub struct VirtualMachineConfig {
    images: Vec<(Vec<u8>, GuestPhysAddr)>,
    memory: u64, // number of 4k pages
}

impl VirtualMachineConfig {
    pub fn new(start_addr: GuestPhysAddr, memory: u64) -> VirtualMachineConfig {
        VirtualMachineConfig {
            images: vec![],
            memory: memory,
        }
    }

    pub fn load_image(&mut self, image: Vec<u8>, addr: GuestPhysAddr) -> Result<()> {
        self.images.push((image, addr));
        Ok(())
    }
}

pub struct VirtualMachine {
    vmcs: vmcs::Vmcs,
    config: VirtualMachineConfig,
    stack: PhysFrame<Size4KiB>,
}

impl VirtualMachine {
    pub fn new(
        vmx: &mut vmx::Vmx,
        alloc: &mut impl FrameAllocator<Size4KiB>,
        config: VirtualMachineConfig,
    ) -> Result<Self> {
        let mut vmcs = vmcs::Vmcs::new(alloc)?;

        let stack = alloc
            .allocate_frame()
            .ok_or(Error::AllocError("Failed to allocate VM stack"))?;

        vmcs.with_active_vmcs(vmx, |mut vmcs| {
            Self::setup_ept(&mut vmcs, alloc)?;
            Self::initialize_host_vmcs(alloc, &mut vmcs, &stack)?;
            Self::initialize_guest_vmcs(&mut vmcs)?;
            Self::initialize_ctrl_vmcs(&mut vmcs, alloc)?;
            Ok(())
        })?;

        Ok(Self {
            vmcs: vmcs,
            config: config,
            stack: stack,
        })
    }

    fn setup_ept(
        vmcs: &mut vmcs::TemporaryActiveVmcs,
        alloc: &mut impl FrameAllocator<Size4KiB>,
    ) -> Result<PhysFrame<Size4KiB>> {
        //FIXME: very hacky ept setup. Just testing for now
        use crate::memory::{self, EptPml4Table};
        use x86_64::structures::paging::FrameAllocator;
        let mut ept_pml4_frame = alloc
            .allocate_frame()
            .expect("Failed to allocate pml4 frame");
        let mut ept_pml4 =
            EptPml4Table::new(&mut ept_pml4_frame).expect("Failed to create pml4 table");

        let mut host_frame = alloc
            .allocate_frame()
            .expect("Failed to allocate host frame");

        memory::map_guest_memory(
            alloc,
            &mut ept_pml4,
            memory::GuestPhysAddr::new(0xFFFFF000),
            host_frame,
            false,
        )?;

        let mut eptp = ept_pml4_frame.start_address().as_u64() ;
        eptp |= 6;// query the bit 8 of the VPID_EPT VMX CAP
        eptp |= (4 - 1) << 3; // page-walk length:4
        eptp |= 1 << 6; // enable acccessed and dirty marking

        vmcs.write_field(vmcs::VmcsField::EptPointer, eptp)?;
        vmcs.write_field(vmcs::VmcsField::VirtualProcessorId, 1)?;

        Ok(ept_pml4_frame)
    }

    fn initialize_host_vmcs(
        alloc: &mut impl FrameAllocator<Size4KiB>,
        vmcs: &mut vmcs::TemporaryActiveVmcs,
        stack: &PhysFrame<Size4KiB>,
    ) -> Result<()> {
        //TODO: Check with MSR_IA32_VMX_CR0_FIXED0/1 that these bits are valid
        vmcs.write_field(vmcs::VmcsField::HostCr0, Cr0::read().bits())?;

        let current_cr3 = Cr3::read();
        vmcs.write_field(
            vmcs::VmcsField::HostCr3,
            current_cr3.0.start_address().as_u64() | current_cr3.1.bits(),
        )?;
        vmcs.write_field(vmcs::VmcsField::HostCr4, Cr4::read())?;

        vmcs.write_field(vmcs::VmcsField::HostEsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostCsSelector, 0xe008)?;
        vmcs.write_field(vmcs::VmcsField::HostSsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostDsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostFsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostGsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostTrSelector, 0x10)?;

        vmcs.write_field(vmcs::VmcsField::HostIa32SysenterCs, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostIa32SysenterEsp, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::HostIa32SysenterEip, 0x00)?;

        vmcs.write_field(vmcs::VmcsField::HostIdtrBase, IdtrBase::read().as_u64())?;
        vmcs.write_field(vmcs::VmcsField::HostGdtrBase, GdtrBase::read().as_u64())?;
        vmcs.write_field(vmcs::VmcsField::HostFsBase, FsBase::read().as_u64())?;
        vmcs.write_field(vmcs::VmcsField::HostGsBase, GsBase::read().as_u64())?;

        let mut tr_base_frame = alloc
            .allocate_frame()
            .expect("Failed to allocate host tr base frame");

        vmcs.write_field(
            vmcs::VmcsField::HostTrBase,
            tr_base_frame.start_address().as_u64(),
        );

        vmcs.write_field(vmcs::VmcsField::HostRsp, stack.start_address().as_u64())?;
        vmcs.write_field(vmcs::VmcsField::HostIa32Efer, Efer::read().bits())?;

        vmcs.write_field(vmcs::VmcsField::HostRip, vmx::vmexit_handler_wrapper as u64)?;

        Ok(())
    }

    fn initialize_guest_vmcs(vmcs: &mut vmcs::TemporaryActiveVmcs) -> Result<()> {
        vmcs.write_field(vmcs::VmcsField::GuestEsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestCsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestSsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestDsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestFsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestGsSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestTrSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestLdtrSelector, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestEsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestCsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestSsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestDsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestFsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestGsBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestTrBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestLdtrBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestIdtrBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestGdtrBase, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestEsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestCsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestSsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestDsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestFsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestGsLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestTrLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestLdtrLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestIdtrLimit, 0xffff)?;
        vmcs.write_field(vmcs::VmcsField::GuestGdtrLimit, 0xffff)?;

        vmcs.write_field(vmcs::VmcsField::GuestEsArBytes, 0xc093)?; // read/write
        vmcs.write_field(vmcs::VmcsField::GuestSsArBytes, 0xc093)?;
        vmcs.write_field(vmcs::VmcsField::GuestDsArBytes, 0xc093)?;
        vmcs.write_field(vmcs::VmcsField::GuestFsArBytes, 0xc093)?;
        vmcs.write_field(vmcs::VmcsField::GuestGsArBytes, 0xc093)?;
        vmcs.write_field(vmcs::VmcsField::GuestCsArBytes, 0xc09b)?; // exec/read

        vmcs.write_field(vmcs::VmcsField::GuestLdtrArBytes, 0x0082)?; // LDT
        vmcs.write_field(vmcs::VmcsField::GuestTrArBytes, 0x008b)?; // TSS (busy)

        vmcs.write_field(vmcs::VmcsField::GuestInterruptibilityInfo, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestActivityState, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestDr7, 0x00)?;
        vmcs.write_field(vmcs::VmcsField::GuestRsp, 0x00)?;

        vmcs.write_field(vmcs::VmcsField::VmcsLinkPointer, 0xffffffff)?;
        vmcs.write_field(vmcs::VmcsField::VmcsLinkPointerHigh, 0xffffffff)?;

        //TODO: get actual EFER (use MSR for vt-x v1)
        vmcs.write_field(vmcs::VmcsField::GuestIa32Efer, 0x00)?;

        let (guest_cr0, guest_cr4) = unsafe {
            let mut cr0_fixed0 = Msr::new(registers::MSR_IA32_VMX_CR0_FIXED0).read();
            cr0_fixed0 &= !(1 << 0); // disable PE
            cr0_fixed0 &= !(1 << 31); // disable PG
            let cr4_fixed0 = Msr::new(registers::MSR_IA32_VMX_CR4_FIXED0).read();
            (cr0_fixed0, cr4_fixed0)
        };
        vmcs.write_field(vmcs::VmcsField::GuestCr0, guest_cr0);
        vmcs.write_field(vmcs::VmcsField::GuestCr4, guest_cr4);

        vmcs.write_field(vmcs::VmcsField::GuestCr3, 0x00)?;

        //TODO: set to a value from the config
        vmcs.write_field(vmcs::VmcsField::GuestRip, 0xFFFFF000)?;

        Ok(())
    }

    fn initialize_ctrl_vmcs(
        vmcs: &mut vmcs::TemporaryActiveVmcs,
        alloc: &mut impl FrameAllocator<Size4KiB>,
    ) -> Result<()> {
        vmcs.write_with_fixed(
            vmcs::VmcsField::CpuBasedVmExecControl,
            vmcs::CpuBasedCtrlFlags::ACTIVATE_SECONDARY_CONTROLS.bits(),
            registers::MSR_IA32_VMX_PROCBASED_CTLS,
        )?;

        vmcs.write_with_fixed(
            vmcs::VmcsField::PinBasedVmExecControl,
            0,
            registers::MSR_IA32_VMX_PINBASED_CTLS,
        )?;

        vmcs.write_with_fixed(
            vmcs::VmcsField::VmExitControls,
            vmcs::VmExitCtrlFlags::IA32E_MODE.bits(),
            registers::MSR_IA32_VMX_EXIT_CTLS,
        )?;

        let field = vmcs.read_field(vmcs::VmcsField::VmExitControls)?;
        info!("Exit Flags: 0x{:x}", field);
        let flags = vmcs::VmExitCtrlFlags::from_bits_truncate(field);
        info!("Exit Flags: {:?}", flags);

        vmcs.write_with_fixed(
            vmcs::VmcsField::VmEntryControls,
            0,
            registers::MSR_IA32_VMX_ENTRY_CTLS,
        )?;

        // vmcs.write_with_fixed(
        //     vmcs::VmcsField::SecondaryVmExecControl,
        //     (vmcs::SecondaryExecFlags::ENABLE_EPT)
        //         .bits(),
        //     registers::MSR_IA32_VMX_PROCBASED_CTLS2,
        // )?;

        let vapic = alloc
            .allocate_frame()
            .ok_or(Error::AllocError("Failed to allocate VAPIC"))?;

        vmcs.write_field(
            vmcs::VmcsField::VirtualApicPageAddr,
            vapic.start_address().as_u64(),
        )?;

        vmcs.write_field(vmcs::VmcsField::ExceptionBitmap, 0xffffffff)?;

        let field = vmcs.read_field(vmcs::VmcsField::CpuBasedVmExecControl)?;
        info!("Flags: 0x{:x}", field);
        let flags = vmcs::CpuBasedCtrlFlags::from_bits_truncate(field);
        info!("Flags: {:?}", flags);

        let field = vmcs.read_field(vmcs::VmcsField::SecondaryVmExecControl)?;
        info!("Secondary Flags: 0x{:x}", field);
        let flags = vmcs::SecondaryExecFlags::from_bits_truncate(field);
        info!("Secondary Flags: {:?}", flags);

        //FIXME: this leaks the bitmap frames
        let bitmap_a = alloc
            .allocate_frame()
            .ok_or(Error::AllocError("Failed to allocate IO bitmap"))?;
        let bitmap_b = alloc
            .allocate_frame()
            .ok_or(Error::AllocError("Failed to allocate IO bitmap"))?;
        vmcs.write_field(
            vmcs::VmcsField::IoBitmapA,
            bitmap_a.start_address().as_u64(),
        )?;
        vmcs.write_field(
            vmcs::VmcsField::IoBitmapB,
            bitmap_b.start_address().as_u64(),
        )?;

        let vapic_frame = alloc
            .allocate_frame()
            .ok_or(Error::AllocError("Failed to allocate VAPIC frame"))?;
        vmcs.write_field(
            vmcs::VmcsField::VirtualApicPageAddr,
            vapic_frame.start_address().as_u64(),
        )?;
        vmcs.write_field(vmcs::VmcsField::TprThreshold, 0)?;

        Ok(())
    }

    pub fn launch(self, vmx: vmx::Vmx) -> Result<!> {
        // TODO: make this and store it in a per-cpu variable
        // Ok(VirtualMachineRunning {
        //     vmcs: self.vmcs.activate(vmx)?,
        // })

        self.vmcs.activate(vmx)?;

        let rflags = unsafe {
            let rflags: u64;
            asm!("vmlaunch; pushfq; popq $0"
                 : "=r"(rflags)
                 :: "rflags"
                 : "volatile");
            rflags
        };

        error::check_vm_insruction(rflags, "Failed to launch vm".into())?;

        unreachable!()
    }
}

pub struct VirtualMachineRunning {
    vmcs: vmcs::ActiveVmcs,
}
