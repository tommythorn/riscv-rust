#![allow(clippy::unreadable_literal)]

/// DRAM base address. Offset from this base address
/// is the address in main memory.
pub const DRAM_BASE: u64 = 0x80000000;

const DTB_SIZE: usize = 0xfe0;

extern crate fnv;

use self::fnv::FnvHashMap;

use crate::cpu::{get_privilege_mode, PrivilegeMode, Trap, TrapType};
use crate::device::clint::Clint;
use crate::device::plic::Plic;
use crate::device::uart::Uart;
use crate::device::virtio_block_disk::VirtioBlockDisk;
use crate::memory::Memory;
use crate::terminal::Terminal;

/// Emulates Memory Management Unit. It holds the Main memory and peripheral
/// devices, maps address to them, and accesses them depending on address.
///
/// It also manages virtual-physical address translation and memoty protection.
/// It may also be said Bus.
/// @TODO: Memory protection is not implemented yet. We should support.
pub struct Mmu {
    clock: u64,
    ppn: u64,
    addressing_mode: AddressingMode,
    privilege_mode: PrivilegeMode,
    memory: MemoryWrapper,
    dtb: Vec<u8>,
    disk: VirtioBlockDisk,
    plic: Plic,
    clint: Clint,
    uart: Uart,

    /// Address translation can be affected `mstatus` (MPRV, MPP in machine mode)
    /// then `Mmu` has copy of it.
    mstatus: u64,

    /// Address translation page cache. Experimental feature.
    /// The cache is cleared when translation mapping can be changed;
    /// xlen, ppn, `privilege_mode`, or `addressing_mode` is updated.
    /// Precisely it isn't good enough because page table entries
    /// can be updated anytime with store instructions, of course
    /// very depending on how pages are mapped tho.
    /// But observing all page table entries is high cost so
    /// ignoring so far. Then this cache optimization can cause a bug
    /// due to unexpected (meaning not in page fault handler)
    /// page table entry update. So this is experimental feature and
    /// disabled by default. If you want to enable, use `enable_page_cache()`.
    page_cache_enabled: bool,
    fetch_page_cache: FnvHashMap<u64, u64>,
    load_page_cache: FnvHashMap<u64, u64>,
    store_page_cache: FnvHashMap<u64, u64>,
}

pub enum AddressingMode {
    None,
    SV39,
    SV48, // @TODO: Implement
}

enum MemoryAccessType {
    Execute,
    Read,
    Write,
    DontCare,
}

const fn _get_addressing_mode_name(mode: &AddressingMode) -> &'static str {
    match mode {
        AddressingMode::None => "None",
        AddressingMode::SV39 => "SV39",
        AddressingMode::SV48 => "SV48",
    }
}

impl Mmu {
    /// Creates a new `Mmu`.
    ///
    /// # Arguments
    /// * `xlen`
    /// * `terminal`
    #[must_use]
    pub fn new(terminal: Box<dyn Terminal>) -> Self {
        let mut dtb = vec![0; DTB_SIZE];

        // Load default device tree binary content
        let content = include_bytes!("./device/dtb.dtb");
        dtb[..content.len()].copy_from_slice(&content[..]);

        Self {
            clock: 0,
            ppn: 0,
            addressing_mode: AddressingMode::None,
            privilege_mode: PrivilegeMode::Machine,
            memory: MemoryWrapper::new(),
            dtb,
            disk: VirtioBlockDisk::new(),
            plic: Plic::new(),
            clint: Clint::new(),
            uart: Uart::new(terminal),
            mstatus: 0,
            page_cache_enabled: false,
            fetch_page_cache: FnvHashMap::default(),
            load_page_cache: FnvHashMap::default(),
            store_page_cache: FnvHashMap::default(),
        }
    }

    /// Initializes Main memory. This method is expected to be called only once.
    ///
    /// # Arguments
    /// * `capacity`
    pub fn init_memory(&mut self, capacity: u64) {
        self.memory.init(capacity);
    }

    /// Initializes Virtio block disk. This method is expected to be called only once.
    ///
    /// # Arguments
    /// * `data` Filesystem binary content
    pub fn init_disk(&mut self, data: &[u8]) {
        self.disk.init(data);
    }

    /// Overrides default Device tree configuration.
    ///
    /// # Arguments
    /// * `data` DTB binary content
    pub fn init_dtb(&mut self, data: &[u8]) {
        self.dtb[..data.len()].copy_from_slice(data);
        for i in data.len()..self.dtb.len() {
            self.dtb[i] = 0;
        }
    }

    /// Enables or disables page cache optimization.
    ///
    /// # Arguments
    /// * `enabled`
    pub fn enable_page_cache(&mut self, enabled: bool) {
        self.page_cache_enabled = enabled;
        self.clear_page_cache();
    }

    /// Clears page cache entries
    fn clear_page_cache(&mut self) {
        self.fetch_page_cache.clear();
        self.load_page_cache.clear();
        self.store_page_cache.clear();
    }

    /// Runs one cycle of MMU and peripheral devices.
    pub fn tick(&mut self, mip: &mut u64) {
        self.clint.tick(mip);
        self.disk.tick(&mut self.memory);
        self.uart.tick();
        self.plic.tick(
            self.disk.is_interrupting(),
            self.uart.is_interrupting(),
            mip,
        );
        self.clock = self.clock.wrapping_add(1);
    }

    /// Updates addressing mode
    ///
    /// # Arguments
    /// * `new_addressing_mode`
    pub fn update_addressing_mode(&mut self, new_addressing_mode: AddressingMode) {
        self.addressing_mode = new_addressing_mode;
        self.clear_page_cache();
    }

    /// Updates privilege mode
    ///
    /// # Arguments
    /// * `mode`
    pub fn update_privilege_mode(&mut self, mode: PrivilegeMode) {
        self.privilege_mode = mode;
        self.clear_page_cache();
    }

    /// Updates mstatus copy. `CPU` needs to call this method whenever
    /// `mstatus` is updated.
    ///
    /// # Arguments
    /// * `mstatus`
    pub fn update_mstatus(&mut self, mstatus: u64) {
        self.mstatus = mstatus;
    }

    /// Updates PPN used for address translation
    ///
    /// # Arguments
    /// * `ppn`
    pub fn update_ppn(&mut self, ppn: u64) {
        self.ppn = ppn;
        self.clear_page_cache();
    }

    /// Fetches an instruction byte. This method takes virtual address
    /// and translates into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    fn fetch(&mut self, v_address: u64) -> Result<u8, Trap> {
        match self.translate_address(v_address, &MemoryAccessType::Execute) {
            Ok(p_address) => Ok(self.load_raw(p_address)),
            Err(()) => Err(Trap {
                trap_type: TrapType::InstructionPageFault,
                value: v_address,
            }),
        }
    }

    /// Fetches instruction four bytes. This method takes virtual address
    /// and translates into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    pub fn fetch_word(&mut self, v_address: u64) -> Result<u32, Trap> {
        let width = 4;
        if v_address & 0xfff <= 0x1000 - width {
            // Fast path. All bytes fetched are in the same page so
            // translating an address only once.
            match self.translate_address(v_address, &MemoryAccessType::Execute) {
                Ok(p_address) => Ok(self.load_word_raw(p_address)),
                Err(()) => Err(Trap {
                    trap_type: TrapType::InstructionPageFault,
                    value: v_address,
                }),
            }
        } else {
            let mut data = 0_u32;
            for i in 0..width {
                match self.fetch(v_address.wrapping_add(i)) {
                    Ok(byte) => data |= u32::from(byte) << (i * 8),
                    Err(e) => return Err(e),
                };
            }
            Ok(data)
        }
    }

    /// Loads an byte. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    pub fn load(&mut self, v_address: u64) -> Result<u8, Trap> {
        match self.translate_address(v_address, &MemoryAccessType::Read) {
            Ok(p_address) => Ok(self.load_raw(p_address)),
            Err(()) => Err(Trap {
                trap_type: TrapType::LoadPageFault,
                value: v_address,
            }),
        }
    }

    /// Loads multiple bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `width` Must be 1, 2, 4, or 8
    fn load_bytes(&mut self, v_address: u64, width: u64) -> Result<u64, Trap> {
        debug_assert!(
            width == 1 || width == 2 || width == 4 || width == 8,
            "Width must be 1, 2, 4, or 8. {width:X}"
        );
        if v_address & 0xfff <= 0x1000 - width {
            match self.translate_address(v_address, &MemoryAccessType::Read) {
                Ok(p_address) => {
                    // Fast path. All bytes fetched are in the same page so
                    // translating an address only once.
                    match width {
                        1 => Ok(u64::from(self.load_raw(p_address))),
                        2 => Ok(u64::from(self.load_halfword_raw(p_address))),
                        4 => Ok(u64::from(self.load_word_raw(p_address))),
                        8 => Ok(self.load_doubleword_raw(p_address)),
                        _ => panic!("Width must be 1, 2, 4, or 8. {width:X}"),
                    }
                }
                Err(()) => Err(Trap {
                    trap_type: TrapType::LoadPageFault,
                    value: v_address,
                }),
            }
        } else {
            let mut data = 0_u64;
            for i in 0..width {
                match self.load(v_address.wrapping_add(i)) {
                    Ok(byte) => data |= u64::from(byte) << (i * 8),
                    Err(e) => return Err(e),
                };
            }
            Ok(data)
        }
    }

    /// Loads two bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    #[allow(clippy::cast_possible_truncation)]
    pub fn load_halfword(&mut self, v_address: u64) -> Result<u16, Trap> {
        match self.load_bytes(v_address, 2) {
            Ok(data) => Ok(data as u16),
            Err(e) => Err(e),
        }
    }

    /// Loads four bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    #[allow(clippy::cast_possible_truncation)]
    pub fn load_word(&mut self, v_address: u64) -> Result<u32, Trap> {
        match self.load_bytes(v_address, 4) {
            Ok(data) => Ok(data as u32),
            Err(e) => Err(e),
        }
    }

    /// Loads eight bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    pub fn load_doubleword(&mut self, v_address: u64) -> Result<u64, Trap> {
        match self.load_bytes(v_address, 8) {
            Ok(data) => Ok(data),
            Err(e) => Err(e),
        }
    }

    /// Store an byte. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `value`
    /// # Errors
    /// Exceptions are returned as errors
    pub fn store(&mut self, v_address: u64, value: u8) -> Result<(), Trap> {
        match self.translate_address(v_address, &MemoryAccessType::Write) {
            Ok(p_address) => {
                self.store_raw(p_address, value);
                Ok(())
            }
            Err(()) => Err(Trap {
                trap_type: TrapType::StorePageFault,
                value: v_address,
            }),
        }
    }

    /// Stores multiple bytes. This method takes a virtual address and translates
    /// it into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `value` data written
    /// * `width` Must be 1, 2, 4, or 8
    /// # Errors
    /// Exceptions are returned as errors
    #[allow(clippy::cast_possible_truncation)]
    fn store_bytes(&mut self, v_address: u64, value: u64, width: u64) -> Result<(), Trap> {
        debug_assert!(
            width == 1 || width == 2 || width == 4 || width == 8,
            "Width must be 1, 2, 4, or 8. {width:X}"
        );
        if v_address & 0xfff <= 0x1000 - width {
            match self.translate_address(v_address, &MemoryAccessType::Write) {
                Ok(p_address) => {
                    // Fast path. All bytes fetched are in the same page so
                    // translating an address only once.
                    match width {
                        1 => self.store_raw(p_address, value as u8),
                        2 => self.store_halfword_raw(p_address, value as u16),
                        4 => self.store_word_raw(p_address, value as u32),
                        8 => self.store_doubleword_raw(p_address, value),
                        _ => panic!("Width must be 1, 2, 4, or 8. {width:X}"),
                    }
                    Ok(())
                }
                Err(()) => Err(Trap {
                    trap_type: TrapType::StorePageFault,
                    value: v_address,
                }),
            }
        } else {
            for i in 0..width {
                match self.store(v_address.wrapping_add(i), ((value >> (i * 8)) & 0xff) as u8) {
                    Ok(()) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    /// Stores two bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `value` data written
    /// # Errors
    /// Exceptions are returned as errors
    pub fn store_halfword(&mut self, v_address: u64, value: u16) -> Result<(), Trap> {
        self.store_bytes(v_address, u64::from(value), 2)
    }

    /// Stores four bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `value` data written
    /// # Errors
    /// Exceptions are returned as errors
    pub fn store_word(&mut self, v_address: u64, value: u32) -> Result<(), Trap> {
        self.store_bytes(v_address, u64::from(value), 4)
    }

    /// Stores eight bytes. This method takes virtual address and translates
    /// into physical address inside.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// * `value` data written
    /// # Errors
    /// Exceptions are returned as errors
    pub fn store_doubleword(&mut self, v_address: u64, value: u64) -> Result<(), Trap> {
        self.store_bytes(v_address, value, 8)
    }

    /// Loads a byte from main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    #[allow(clippy::cast_possible_truncation)]
    fn load_raw(&mut self, p_address: u64) -> u8 {
        // @TODO: Mapping should be configurable with dtb
        if p_address >= DRAM_BASE {
            self.memory.read_byte(p_address)
        } else {
            match p_address {
                // I don't know why but dtb data seems to be stored from 0x1020 on Linux.
                // It might be from self.x[0xb] initialization?
                // And DTB size is arbitray.
                0x00001020..=0x00001fff => self.dtb[p_address as usize - 0x1020],
                0x02000000..=0x0200ffff => self.clint.load(p_address),
                0x0C000000..=0x0fffffff => self.plic.load(p_address),
                0x10000000..=0x100000ff => self.uart.load(p_address),
                0x10001000..=0x10001FFF => self.disk.load(p_address),
                _ => panic!("Unknown memory mapping {p_address:X}."),
            }
        }
    }

    /// Loads two bytes from main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    fn load_halfword_raw(&mut self, p_address: u64) -> u16 {
        if p_address >= DRAM_BASE && p_address.wrapping_add(1) > p_address {
            // Fast path. Directly load main memory at a time.
            self.memory.read_halfword(p_address)
        } else {
            let mut data = 0_u16;
            for i in 0..2 {
                data |= u16::from(self.load_raw(p_address.wrapping_add(i))) << (i * 8);
            }
            data
        }
    }

    /// Loads four bytes from main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    pub fn load_word_raw(&mut self, p_address: u64) -> u32 {
        if p_address >= DRAM_BASE && p_address.wrapping_add(3) > p_address {
            self.memory.read_word(p_address)
        } else {
            let mut data = 0_u32;
            for i in 0..4 {
                data |= u32::from(self.load_raw(p_address.wrapping_add(i))) << (i * 8);
            }
            data
        }
    }

    /// Loads eight bytes from main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    fn load_doubleword_raw(&mut self, p_address: u64) -> u64 {
        if p_address >= DRAM_BASE && p_address.wrapping_add(7) > p_address {
            self.memory.read_doubleword(p_address)
        } else {
            let mut data = 0_u64;
            for i in 0..8 {
                data |= u64::from(self.load_raw(p_address.wrapping_add(i))) << (i * 8);
            }
            data
        }
    }

    /// Stores a byte to main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    /// * `value` data written
    /// # Panics
    /// Will panic on access to unsupported MMIO ranges (XXX this should just ignore them)
    pub fn store_raw(&mut self, p_address: u64, value: u8) {
        // @TODO: Mapping should be configurable with dtb
        if p_address >= DRAM_BASE {
            self.memory.write_byte(p_address, value);
        } else {
            match p_address {
                0x02000000..=0x0200ffff => self.clint.store(p_address, value),
                0x0c000000..=0x0fffffff => self.plic.store(p_address, value),
                0x10000000..=0x100000ff => self.uart.store(p_address, value),
                0x10001000..=0x10001FFF => self.disk.store(p_address, value),
                _ => panic!("Unknown memory mapping {p_address:X}."),
            }
        };
    }

    /// Stores two bytes to main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    /// * `value` data written
    fn store_halfword_raw(&mut self, p_address: u64, value: u16) {
        if p_address >= DRAM_BASE && p_address.wrapping_add(1) > p_address {
            self.memory.write_halfword(p_address, value);
        } else {
            for i in 0..2 {
                self.store_raw(p_address.wrapping_add(i), ((value >> (i * 8)) & 0xff) as u8);
            }
        }
    }

    /// Stores four bytes to main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    /// * `value` data written
    fn store_word_raw(&mut self, p_address: u64, value: u32) {
        if p_address >= DRAM_BASE && p_address.wrapping_add(3) > p_address {
            self.memory.write_word(p_address, value);
        } else {
            for i in 0..4 {
                self.store_raw(p_address.wrapping_add(i), ((value >> (i * 8)) & 0xff) as u8);
            }
        }
    }

    /// Stores eight bytes to main memory or peripheral devices depending on
    /// physical address.
    ///
    /// # Arguments
    /// * `p_address` Physical address
    /// * `value` data written
    fn store_doubleword_raw(&mut self, p_address: u64, value: u64) {
        if p_address >= DRAM_BASE && p_address.wrapping_add(7) > p_address {
            self.memory.write_doubleword(p_address, value);
        } else {
            for i in 0..8 {
                self.store_raw(p_address.wrapping_add(i), ((value >> (i * 8)) & 0xff) as u8);
            }
        }
    }

    /// Checks if passed virtual address is valid (pointing a certain device) or not.
    /// This method can return page fault trap.
    ///
    /// # Arguments
    /// * `v_address` Virtual address
    /// # Errors
    /// Exceptions are returned as errors
    #[allow(clippy::result_unit_err)] // @TODO: broken mess of Result usage
    pub fn validate_address(&mut self, v_address: u64) -> Result<bool, ()> {
        // @TODO: Support other access types?
        let p_address = self.translate_address(v_address, &MemoryAccessType::DontCare)?;
        let valid = if p_address >= DRAM_BASE {
            self.memory.validate_address(p_address)
        } else {
            matches!(p_address, 0x00001020..=0x00001fff |
		     0x02000000..=0x0200ffff |
		     0x0c000000..=0x0fffffff |
		     0x10000000..=0x100000ff |
		     0x10001000..=0x10001fff)
        };
        Ok(valid)
    }

    fn translate_address(
        &mut self,
        v_address: u64,
        access_type: &MemoryAccessType,
    ) -> Result<u64, ()> {
        let address = v_address;
        let v_page = address & !0xfff;
        let cache = if self.page_cache_enabled {
            match access_type {
                MemoryAccessType::Execute => self.fetch_page_cache.get(&v_page),
                MemoryAccessType::Read => self.load_page_cache.get(&v_page),
                MemoryAccessType::Write => self.store_page_cache.get(&v_page),
                MemoryAccessType::DontCare => None,
            }
        } else {
            None
        };
        if let Some(p_page) = cache {
            Ok(p_page | (address & 0xfff))
        } else {
            let p_address = match self.addressing_mode {
                AddressingMode::None => Ok(address),
                AddressingMode::SV39 => match self.privilege_mode {
                    // @TODO: Optimize
                    // @TODO: Remove duplicated code with SV32
                    PrivilegeMode::Machine => match access_type {
                        MemoryAccessType::Execute => Ok(address),
                        // @TODO: Remove magic number
                        _ => {
                            if (self.mstatus >> 17) & 1 == 0 {
                                Ok(address)
                            } else {
                                let privilege_mode = get_privilege_mode((self.mstatus >> 9) & 3);
                                if matches!(privilege_mode, PrivilegeMode::Machine) {
                                    Ok(address)
                                } else {
                                    let current_privilege_mode = self.privilege_mode;
                                    self.update_privilege_mode(privilege_mode);
                                    let result = self.translate_address(v_address, access_type);
                                    self.update_privilege_mode(current_privilege_mode);
                                    result
                                }
                            }
                        }
                    },
                    PrivilegeMode::User | PrivilegeMode::Supervisor => {
                        let vpns = [
                            (address >> 12) & 0x1ff,
                            (address >> 21) & 0x1ff,
                            (address >> 30) & 0x1ff,
                        ];
                        self.traverse_page(address, 3 - 1, self.ppn, &vpns, access_type)
                    }
                    PrivilegeMode::Reserved => Ok(address),
                },
                AddressingMode::SV48 => {
                    panic!("AddressingMode SV48 is not supported yet.");
                }
            };
            if self.page_cache_enabled {
                match p_address {
                    Ok(p_address) => {
                        let p_page = p_address & !0xfff;
                        match access_type {
                            MemoryAccessType::Execute => {
                                self.fetch_page_cache.insert(v_page, p_page)
                            }
                            MemoryAccessType::Read => self.load_page_cache.insert(v_page, p_page),
                            MemoryAccessType::Write => self.store_page_cache.insert(v_page, p_page),
                            MemoryAccessType::DontCare => None,
                        };
                        Ok(p_address)
                    }
                    Err(()) => Err(()),
                }
            } else {
                p_address
            }
        }
    }

    #[allow(
        clippy::many_single_char_names,
        clippy::too_many_lines,
        clippy::cast_possible_truncation
    )]
    fn traverse_page(
        &mut self,
        v_address: u64,
        level: u8,
        parent_ppn: u64,
        vpns: &[u64],
        access_type: &MemoryAccessType,
    ) -> Result<u64, ()> {
        let pagesize = 4096;
        let ptesize = 8;
        let pte_address = parent_ppn * pagesize + vpns[level as usize] * ptesize;
        let pte = self.load_doubleword_raw(pte_address);
        let ppn = (pte >> 10) & 0xfffffffffff;
        let ppns = if matches!(self.addressing_mode, AddressingMode::SV39) {
            [
                (pte >> 10) & 0x1ff,
                (pte >> 19) & 0x1ff,
                (pte >> 28) & 0x3ffffff,
            ]
        } else {
            unreachable!()
        };
        let d = (pte >> 7) & 1;
        let a = (pte >> 6) & 1;
        let x = (pte >> 3) & 1;
        let w = (pte >> 2) & 1;
        let r = (pte >> 1) & 1;
        let v = pte & 1;

        // println!("VA:{:X} Level:{:X} PTE_AD:{:X} PTE:{:X} PPPN:{:X} PPN:{:X} PPN1:{:X} PPN0:{:X}", v_address, level, pte_address, pte, parent_ppn, ppn, ppns[1], ppns[0]);

        if v == 0 || (r == 0 && w == 1) {
            return Err(());
        }

        if r == 0 && x == 0 {
            return match level {
                0 => Err(()),
                _ => self.traverse_page(v_address, level - 1, ppn, vpns, access_type),
            };
        }

        // Leaf page found

        if a == 0
            || (match access_type {
                MemoryAccessType::Write => d == 0,
                _ => false,
            })
        {
            let new_pte = pte
                | (1 << 6)
                | (match access_type {
                    MemoryAccessType::Write => 1 << 7,
                    _ => 0,
                });
            self.store_doubleword_raw(pte_address, new_pte);
        }

        if match access_type {
            MemoryAccessType::Execute => x == 0,
            MemoryAccessType::Read => r == 0,
            MemoryAccessType::Write => w == 0,
            MemoryAccessType::DontCare => false,
        } {
            return Err(());
        }

        let offset = v_address & 0xfff; // [11:0]
                                        // @TODO: Optimize
        let p_address = match level {
            2 => {
                if ppns[1] != 0 || ppns[0] != 0 {
                    return Err(());
                }
                (ppns[2] << 30) | (vpns[1] << 21) | (vpns[0] << 12) | offset
            }
            1 => {
                if ppns[0] != 0 {
                    return Err(());
                }
                (ppns[2] << 30) | (ppns[1] << 21) | (vpns[0] << 12) | offset
            }
            0 => (ppn << 12) | offset,
            _ => panic!(), // Shouldn't happen
        };

        // println!("PA:{:X}", p_address);
        Ok(p_address)
    }

    /// Returns immutable reference to `Clint`.
    #[must_use]
    pub const fn get_clint(&self) -> &Clint {
        &self.clint
    }

    /// Returns mutable reference to `Clint`.
    pub fn get_mut_clint(&mut self) -> &mut Clint {
        &mut self.clint
    }

    /// Returns mutable reference to `Uart`.
    pub fn get_mut_uart(&mut self) -> &mut Uart {
        &mut self.uart
    }
}

/// [`Memory`](../memory/struct.Memory.html) wrapper. Converts physical address to the one in memory
/// using [`DRAM_BASE`](constant.DRAM_BASE.html) and accesses [`Memory`](../memory/struct.Memory.html).
pub struct MemoryWrapper {
    memory: Memory,
}

impl MemoryWrapper {
    const fn new() -> Self {
        Self {
            memory: Memory::new(),
        }
    }

    fn init(&mut self, capacity: u64) {
        self.memory.init(capacity);
    }

    pub fn read_byte(&mut self, p_address: u64) -> u8 {
        debug_assert!(
            p_address >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.read_byte(p_address - DRAM_BASE)
    }

    pub fn read_halfword(&mut self, p_address: u64) -> u16 {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(1) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.read_halfword(p_address - DRAM_BASE)
    }

    pub fn read_word(&mut self, p_address: u64) -> u32 {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(3) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.read_word(p_address - DRAM_BASE)
    }

    pub fn read_doubleword(&mut self, p_address: u64) -> u64 {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(7) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.read_doubleword(p_address - DRAM_BASE)
    }

    pub fn write_byte(&mut self, p_address: u64, value: u8) {
        debug_assert!(
            p_address >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.write_byte(p_address - DRAM_BASE, value);
    }

    pub fn write_halfword(&mut self, p_address: u64, value: u16) {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(1) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.write_halfword(p_address - DRAM_BASE, value);
    }

    pub fn write_word(&mut self, p_address: u64, value: u32) {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(3) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.write_word(p_address - DRAM_BASE, value);
    }

    pub fn write_doubleword(&mut self, p_address: u64, value: u64) {
        debug_assert!(
            p_address >= DRAM_BASE && p_address.wrapping_add(7) >= DRAM_BASE,
            "Memory address must equals to or bigger than DRAM_BASE. {p_address:X}"
        );
        self.memory.write_doubleword(p_address - DRAM_BASE, value);
    }

    #[must_use]
    pub fn validate_address(&self, address: u64) -> bool {
        self.memory.validate_address(address - DRAM_BASE)
    }
}
