extern crate alloc;

use crate::pe_helper::pe_def::{
    ASCIIString, ImageDataDirectory, ImageDataDirectoryEntry, ImageDataDirectoryInfo,
    ImageDataDirectoryVec, ImportAddressEntry, PEType, Pe64C, UnicodeString, PE64, RVA32,
};
use alloc::sync::Arc;
use core::arch::asm;
use core::mem::transmute;
use iced_x86::{Decoder, DecoderOptions, Instruction, OpKind, Register};
use std::sync::Mutex;
use windows::core::PCSTR;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};

use crate::pe_def;

/// Error type for the PE helper
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PEHelperError {
    LockFailure,
    ModuleNotFound,
    VirtualProtectFailed,
    PEFileNotParsed,
    IATNotFound,
    AddressNotWithinModuleRange,
    ExportNameNotFound,
    ExportOrdinalNotFound,
    ExportAddressNotFound,
    ExportIsFowarder,
    ExportDirectoryTableNotFound,
    PeAlreadyParsed,
    InvalidDosSignature,
    InvalidPeSignature,
    UnhandledPeType,
    InvalidNumberOfDataDirectoryEntries,
}

// Global mutable vec of ModuleHandles using Mutex and Arc
static MODULES: Mutex<Vec<Arc<ModuleHandle64>>> = Mutex::new(Vec::new());

/// Attempts to unhook the IAT of the current process, returns Ok(true) if successful,
/// Ok(false) if no hooks were detected, or Err if an error occurred.
pub fn unpatch_iat_hooks(hmod: &ModuleHandle64) -> Result<bool, PEHelperError> {
    if !hmod.is_pe_parsed()? {
        // Parse the pe if its not already parsed
        hmod.parse_pe()?;
    }
    // We only unhook ntdll, ensure all unhooked entries
    // fall within ntdll
    let ntdll_str = "ntdll.dll\0";
    let ntdll_pcstr = PCSTR::from_raw(ntdll_str.as_ptr());
    let base_address_ntdll = match unsafe { GetModuleHandleA(ntdll_pcstr) } {
        Ok(base_address) => base_address,
        Err(_) => return Err(PEHelperError::ModuleNotFound),
    };
    // Get a parsed PE file from the base address of ntdll
    let pe_file_ntdll = match get_module_by_address(base_address_ntdll.0 as usize) {
        Some(pe_file) => pe_file,
        None => return Err(PEHelperError::ModuleNotFound),
    };
    // Get the start and end of ntdll which we use to check if the target of the iat entry is
    // within ntdll
    let ntdll_start = pe_file_ntdll.get_base_address();
    let ndtll_end = ntdll_start + pe_file_ntdll.get_size();

    // Get exclusive access to the pe file
    let pe_lock = hmod.pe.lock().map_err(|_| PEHelperError::LockFailure)?;
    let pe = match pe_lock.as_ref() {
        Some(pe) => pe,
        None => return Err(PEHelperError::PEFileNotParsed),
    };

    // Get access to the import directory table
    let iat = match pe.get_data_directories().get_import_address_table() {
        Some(iat) => iat,
        None => return Err(PEHelperError::IATNotFound),
    };
    if iat.addresses.is_empty() {
        // No iat? No hooks
        return Ok(false);
    }
    let mut patched = false;
    // Loop through the iat, and check if any of the entries are hooked
    println!("Looping through iat addresses");
    for iat_entry in iat.addresses.iter() {
        // Ensure the target is within ntdll, otherwise its another module or potentially uninitialized
        if iat_entry.target_function_address < (ntdll_start as u64)
            || iat_entry.target_function_address > (ndtll_end as u64)
        {
            continue;
        }
        println!(
            "IAT address :{:#x}, target: {:#x}",
            iat_entry.iat_entry_address, iat_entry.target_function_address
        );
        let result = unhook_iat_entry(iat_entry)?;
        if result {
            println!(
                "Unhooked iat entry at {:x}, orig_target:{:#x}",
                iat_entry.iat_entry_address, iat_entry.target_function_address
            );
            patched = true;
        }
    }
    Ok(patched)
}

/// Takes an iat_entry, disassembles the target function to determine if its hooked,
/// if it is, it will unhook it and return Ok(true), if it is not hooked, it will return Ok(false),
/// if an error occurs, it will return Err.
fn unhook_iat_entry(iat_entry: &ImportAddressEntry) -> Result<bool, PEHelperError> {
    let target_addr = iat_entry.target_function_address;
    let replace_addr = iat_entry.iat_entry_address;
    // Ensure the addresses are at least non-zero
    if target_addr == 0 || replace_addr == 0 {
        return Ok(false);
    }

    // Define the buffer of bytes to disassemble, we will disassemble 512-bytes at a time.
    // This is unsafe as we do little to no bounds checking.

    let target_len = 512;

    // We loop until we patch the hook or reach the end of our analysis for this target address.
    let target_bytes = unsafe { std::slice::from_raw_parts(target_addr as *const u8, target_len) };
    // Disassemble
    let mut decoder = Decoder::with_ip(
        64,
        target_bytes,
        target_addr,
        DecoderOptions::NO_INVALID_CHECK,
    );
    // Determine what the opcode is for the first instruction
    let mut instruction = Instruction::default();
    // Decode the first instruction
    decoder.decode_out(&mut instruction);
    // If its not a Jmp, we assume its not hooked
    if instruction.code().op_code().mnemonic() != iced_x86::Mnemonic::Jmp {
        return Ok(false);
    }
    println!("Found hook\n");
    // Now change the decoder to decode the target of the jmp
    let target = instruction.near_branch_target();
    println!("Stage 1 jmp target: {:#x}", target);
    let target_bytes = unsafe { std::slice::from_raw_parts(target as *const u8, target_len) };
    decoder = Decoder::with_ip(64, target_bytes, target, DecoderOptions::NO_INVALID_CHECK);

    // This should be the second stage jmp
    instruction = Instruction::default();
    // Decode the second instruction
    decoder.decode_out(&mut instruction);
    // Support cases where its *not* a jmp, but if it is, we need to follow it
    if instruction.code().op_code().mnemonic() == iced_x86::Mnemonic::Jmp {
        // We expect this to be a jmp[mem], so we get the memory location, deref it and start a new decoder
        // at the new target
        let target_ptr = unsafe { &*(instruction.memory_displacement64() as *const u64) };
        println!("Stage 2 jmp target: {:#x}", *target_ptr);
        let target_bytes =
            unsafe { std::slice::from_raw_parts(*target_ptr as *const u8, target_len) };
        decoder = Decoder::with_ip(
            64,
            target_bytes,
            *target_ptr,
            DecoderOptions::NO_INVALID_CHECK,
        );
    }

    // Continue decoding instructions until we reach the end of the bytes or hit an end condition
    // (ret or syscall), if the instruction is a syscall then we assume the target is not hooked.
    let mut instr_since_rax = 0;
    // Keep track of the last rax indirect load, we expect this to be the address of the syscall stub
    // we want to restore when we find the target indirect call
    let mut rax_indirect_load = None;
    instruction = Instruction::default();
    while decoder.can_decode() {
        // Decode the next instruction
        decoder.decode_out(&mut instruction);

        // Check if the instruction is a syscall or a ret, in which case we assume its not hooked
        if instruction.code().op_code().mnemonic() == iced_x86::Mnemonic::Syscall
            || instruction.code().op_code().mnemonic() == iced_x86::Mnemonic::Ret
        {
            println!("Found syscall or ret\n");
            return Ok(false);
        }

        if instruction.op_kind(0) == OpKind::Register && instruction.op_register(0) == Register::RAX
        {
            if instruction.op_kind(1) == OpKind::Memory {
                println!("Found RAX indirect load");
                rax_indirect_load = Some(instruction.memory_displacement64());
                instr_since_rax = 1;
            } else {
                // Other operation on RAX, this may not actually modify RAX but its unexpected
                // so lets reset the counter
                instr_since_rax = 0;
                rax_indirect_load = None;
            }
        }
        // Not expecting more than 8 instructions between the RAX indirect load and the indirect call
        if instr_since_rax > 8 {
            instr_since_rax = 0;
            rax_indirect_load = None;
        }
        if instr_since_rax > 0 {
            instr_since_rax += 1;
        }
        // Check if the instruction is an indirect call
        if instruction.is_call_far_indirect() || instruction.is_call_near_indirect() {
            println!("Found indirect far call");
            return match rax_indirect_load {
                Some(rax_indirect_load) => {
                    // We have a potential match
                    // Patch the IAT
                    // First, call virtual_protect to make the page writable
                    let mut old_protect = PAGE_PROTECTION_FLAGS(0);
                    let result = unsafe {
                        VirtualProtect(replace_addr as *mut _, 8, PAGE_READWRITE, &mut old_protect)
                    };
                    if !result.as_bool() {
                        return Err(PEHelperError::VirtualProtectFailed);
                    }
                    // Now we can patch the iat entry
                    let iat_entry_ptr = replace_addr as *mut u64;
                    // Get the actual target address
                    let rax_indirect_load = unsafe { *(rax_indirect_load as *const u64) };
                    unsafe { *iat_entry_ptr = rax_indirect_load };
                    // Now we can call virtual_protect to restore the old protection
                    let result = unsafe {
                        VirtualProtect(replace_addr as *mut _, 8, old_protect, &mut old_protect)
                    };
                    if !result.as_bool() {
                        return Err(PEHelperError::VirtualProtectFailed);
                    }
                    // Done, return
                    Ok(true)
                }
                None => {
                    println!("Found indirect call, but RAX was not loaded with a global pointer");
                    // Not expecting any other indirect calls, so we assume the target is not hooked
                    Ok(false)
                }
            };
        }
        instruction = Instruction::default();
    }
    // If we reached here, we didn't find the hook
    Ok(false)
}

/// Custom GetProcAddress implementation by parsing the export directory table of a PE64 module
pub fn get_proc_address(
    hmod: &ModuleHandle64,
    target_func_name: &str,
) -> Result<extern "C" fn(), PEHelperError> {
    if !hmod.is_pe_parsed()? {
        // Parse the pe if its not already parsed
        hmod.parse_pe()?;
    }
    // At this point we guarantee the pe in the module handle is parsed
    let pe_lock = hmod.pe.lock().map_err(|_| PEHelperError::LockFailure)?;
    let pe = match pe_lock.as_ref() {
        Some(pe) => pe,
        None => return Err(PEHelperError::PEFileNotParsed),
    };
    // Get the export table so we can get the address of the export name pointer table and find
    // the index of the target function name
    let export_directory_table = match pe.get_data_directories().get_export_table() {
        Some(export_directory_table) => export_directory_table,
        None => return Err(PEHelperError::ExportDirectoryTableNotFound),
    };
    let ordinal_table_index = match export_directory_table
        .get_export_name_ptr_table_entry(target_func_name, pe.base_address)
    {
        Some(ordinal_table_index) => ordinal_table_index,
        None => return Err(PEHelperError::ExportNameNotFound),
    };
    let export_addr_index = match export_directory_table
        .get_export_ordinal_table_entry(ordinal_table_index, pe.base_address)
    {
        Some(export_addr_index) => export_addr_index,
        None => return Err(PEHelperError::ExportOrdinalNotFound),
    };
    let export_addr = match export_directory_table
        .get_export_address_table_entry(*export_addr_index, pe.base_address)
    {
        Some(export_addr) => export_addr,
        None => return Err(PEHelperError::ExportAddressNotFound),
    };
    // Ensure the export address is within the module range
    let export_absolute = export_addr.0.get(pe.base_address) as *const _ as usize;
    let is_within_mod_range = {
        let mod_base = hmod.get_base_address();
        let mod_end = mod_base + hmod.get_size();
        export_absolute >= mod_base && export_absolute < mod_end
    };
    if !is_within_mod_range {
        return Err(PEHelperError::AddressNotWithinModuleRange);
    }
    // Determine if the export address is a forwarder by checking if its within range of the module's
    // export table
    let is_forwarder = pe
        .get_data_directories()
        .is_within_range(ImageDataDirectoryEntry::ExportTable, export_absolute)
        .unwrap();
    if is_forwarder {
        // Get the forwarder ASCII string and print it, we don't support forwarders
        let forwarder: &ASCIIString = unsafe { &*(export_absolute as *const ASCIIString) };
        let _forwarder_str = forwarder.to_string();
        return Err(PEHelperError::ExportIsFowarder);
    }
    // At this point we know the export address is not a forwarder
    Ok(unsafe { transmute(export_absolute) })
}

/// Finds a [`ModuleHandle`] in the PEB that matches the provided name.
/// Returns a handle to the module if found, otherwise returns None.
pub fn get_module_by_name(target_module_name: &str) -> Option<Arc<ModuleHandle64>> {
    // Search MODULES for an existing ModuleHandle with the same name
    // If found, return it
    // If not found, create a new ModuleHandle and return it
    let mut mod_array = match MODULES.lock() {
        Ok(mod_array) => mod_array,
        Err(_err) => return None,
    };
    for module in mod_array.iter() {
        if module.name.to_lowercase() == target_module_name.to_lowercase() {
            return Some(module.clone());
        }
    }
    let peb = match get_peb() {
        Some(peb) => peb,
        None => return None,
    };
    let ldr = unsafe { core::ptr::read_volatile(peb.ldr) };
    let mut current = ldr.in_memory_order_module_list.flink;
    // loop through in_memory_order_module_list
    loop {
        // adjust current pointer by subtracting the size of a ListEntry from it
        current = unsafe { current.sub(1) };
        let current_module = unsafe { &*(current as *const LdrDataTableEntry) };
        let current_module_name = match current_module.base_dll_name.extract_string() {
            Some(name) => name,
            None => return None,
        };
        if current_module_name.to_lowercase() == target_module_name.to_lowercase() {
            let module_handle = Arc::new(ModuleHandle64 {
                name: current_module_name,
                base: current_module.dll_base,
                size: current_module.size_of_image,
                pe: Mutex::new(None),
            });
            mod_array.push(module_handle.clone());
            return Some(module_handle);
        }
        current = current_module.in_memory_order_links.flink;
        if current == ldr.in_memory_order_module_list.flink {
            return None;
        }
    }
}

/// Finds a [`ModuleHandle`] in the PEB that matches the provided base address.
/// Returns a handle to the module if found, otherwise returns None.
pub fn get_module_by_address(target_module_address: usize) -> Option<Arc<ModuleHandle64>> {
    // Search MODULES for an existing ModuleHandle with the same address
    // If found, return it
    // If not found, create a new ModuleHandle and return it
    let mut mod_array = match MODULES.lock() {
        Ok(mod_array) => mod_array,
        Err(_err) => return None,
    };
    for module in mod_array.iter() {
        if module.base == target_module_address {
            return Some(module.clone());
        }
    }
    let peb = match get_peb() {
        Some(peb) => peb,
        None => return None,
    };
    let ldr = unsafe { core::ptr::read_volatile(peb.ldr) };
    let mut current = ldr.in_memory_order_module_list.flink;
    // loop through in_memory_order_module_list
    loop {
        // adjust current pointer by subtracting the size of a ListEntry from it
        current = unsafe { current.sub(1) };
        let current_module = unsafe { &*(current as *const LdrDataTableEntry) };
        let current_module_name = match current_module.base_dll_name.extract_string() {
            Some(name) => name,
            None => return None,
        };
        if current_module.dll_base == target_module_address {
            let module_handle = Arc::new(ModuleHandle64 {
                name: current_module_name,
                base: current_module.dll_base,
                size: current_module.size_of_image,
                pe: Mutex::new(None),
            });
            mod_array.push(module_handle.clone());
            return Some(module_handle);
        }
        current = current_module.in_memory_order_links.flink;
        if current == ldr.in_memory_order_module_list.flink {
            return None;
        }
    }
}

fn get_peb() -> Option<PEB> {
    // Check if GS register is null
    unsafe {
        let gs: usize;
        asm!("mov {}, gs", out(reg) gs, options(nomem, nostack));
        if gs == 0 {
            return None;
        }
    }
    let peb: *const PEB;
    unsafe {
        asm!("mov {}, gs:[0x60]", out(reg) peb);
        Some(core::ptr::read_volatile(peb))
    }
}

/// A handle to a loaded module.
pub struct ModuleHandle64 {
    /// The name of the module.
    name: String,
    /// The base address of the module.
    base: usize,
    /// The size of the module.
    size: usize,
    /// An optional parsed PE representation of the module
    pe: Mutex<Option<Box<PE64>>>,
}

impl ModuleHandle64 {
    /// Returns the name of the module.
    pub fn get_name(&self) -> &str {
        &self.name
    }
    /// Returns the base field of the module.
    pub fn get_base_address(&self) -> usize {
        self.base
    }
    /// Returns the size field of the module
    pub fn get_size(&self) -> usize {
        self.size
    }
    /// Parses the pe file represented by this module, stores the result
    /// in the associated pe variable and returns a result
    pub fn parse_pe(&self) -> Result<(), PEHelperError> {
        // Get a lock to the pe field, if the lock fails return an error
        let mut pe_self = self.pe.lock().map_err(|_| PEHelperError::LockFailure)?;
        // If the pe is already populated, return an error
        if pe_self.is_some() {
            return Err(PEHelperError::PeAlreadyParsed);
        }
        // Get the base address of the module
        let base_address = self.base;
        // Get the base address as a pointer to a PE64
        let pe_header = unsafe { &*(base_address as *const Pe64C) };
        // Verify the DOS signature is valid
        if !pe_header.dos_header.e_magic.is_valid() {
            return Err(PEHelperError::InvalidDosSignature);
        }
        // Get the NT headers from the pe_header e_lfanew
        let nt_headers = pe_header.get_nt_headers();
        // Verify the PE signature is valid
        if !nt_headers.signature.is_valid() {
            return Err(PEHelperError::InvalidPeSignature);
        }
        // Verify the PE type is valid
        if nt_headers.optional_header.magic != PEType::PE64 {
            return Err(PEHelperError::UnhandledPeType);
        }
        // parse the data directory entries and create the pe struct
        let num_data_directory_entries =
            nt_headers.optional_header.number_of_rva_and_sizes as usize;
        // Validate that the number of data directory entries is reasonable
        if num_data_directory_entries > 16 {
            return Err(PEHelperError::InvalidNumberOfDataDirectoryEntries);
        }
        // Create a vec of data directory entries from the PE header
        let data_directory_entries = unsafe {
            core::slice::from_raw_parts(
                &nt_headers.optional_header.data_directory as *const _ as *const ImageDataDirectory,
                num_data_directory_entries,
            )
        };
        // Turn data_directory_entries into a vec of ImageDataDirectoryInfo, the ImageDataDirectoryInfo.name
        // is obtained from the index of the data_directory_entries array
        let data_directory_info = data_directory_entries
            .iter()
            .enumerate()
            .map(|(i, entry)| ImageDataDirectoryInfo {
                virtual_address: RVA32::<()>(entry.virtual_address, core::marker::PhantomData),
                size: entry.size,
                base_address,
                name: ImageDataDirectoryEntry::from_index(i).unwrap(),
            })
            .collect::<Vec<_>>();

        let pe = Box::new(PE64 {
            pe64: Box::new((*pe_header).clone()),
            base_address,
            data_directories: ImageDataDirectoryVec(data_directory_info),
        });
        // Lock the pe field and set it to the parsed PE
        pe_self.replace(pe);
        Ok(())
    }
    /// Checks if the pe field is populated and returns a result with errors if
    /// the pe field is not populated, or if the lock failed to be obtained
    pub fn is_pe_parsed(&self) -> Result<bool, PEHelperError> {
        let pe_self = self.pe.lock().map_err(|_| PEHelperError::LockFailure)?;
        Ok(pe_self.is_some())
    }
}

// Create tests for this library
#[cfg(test)]
mod tests {
    use super::*;

    /// Get the 64-bit PEB
    #[test]
    fn test_get_peb() {
        let peb = get_peb();
        assert!(peb.is_some());
    }

    /// Get a module handle by name
    #[test]
    fn test_get_module_by_name() {
        let hmod = get_module_by_name("kernel32.dll");
        assert!(hmod.is_some());
    }
    /// Get multiple handles to the same module and verify the ref_count increase
    #[test]
    fn test_get_module_by_name_ref_count() {
        let hmod = get_module_by_name("kernel32.dll");
        assert!(hmod.is_some());
        // Assert hmod Arc ref_count is 2
        assert_eq!(Arc::strong_count(hmod.as_ref().unwrap()), 2);
        let hmod2 = get_module_by_name("kernel32.dll");
        assert!(hmod2.is_some());
        // Assert ref_count is 3
        assert_eq!(Arc::strong_count(hmod.as_ref().unwrap()), 3);
        drop(hmod2.unwrap());
        // Assert ref_count is 2
        assert_eq!(Arc::strong_count(hmod.as_ref().unwrap()), 2);
    }
    /// Test parsing a module as a PE file
    #[test]
    fn test_parse_pe() {
        let hmod = get_module_by_name("kernelbase.dll").unwrap();
        assert!(hmod.is_pe_parsed() == Ok(false));
        let res = hmod.parse_pe();
        assert!(res.is_ok(), "Failed to parse PE: {:#?}", res.err().unwrap());
        // assert that hmod.pe is populated
        assert!(hmod.is_pe_parsed() == Ok(true));
        // Assert that attempting to parse it again returns an error
        assert!(hmod.parse_pe().is_err());
    }
    /// Tests finding a function pointer by obtaining the ExportDirectoryTable and searching for
    /// the function name in the ExportNameTable, using its index in the ExportAddressTable and
    /// checking if the range is within the Export section
    #[test]
    fn get_ntdll_ntopenfile_function() {
        let target_function = "NtOpenFile";
        let hmod = get_module_by_name("ntdll.dll").unwrap();
        let res = hmod.parse_pe();
        assert!(res.is_ok(), "Failed to parse PE: {:#?}", res.err().unwrap());
        let pe_lock = hmod.pe.lock().unwrap();
        let pe = pe_lock.as_ref().unwrap();
        let export_directory_table = pe.get_data_directories().get_export_table().unwrap();
        let ordinal_table_index = export_directory_table
            .get_export_name_ptr_table_entry(target_function, pe.base_address)
            .unwrap();
        let export_addr_index = export_directory_table
            .get_export_ordinal_table_entry(ordinal_table_index, pe.base_address)
            .unwrap();
        let export_addr = export_directory_table
            .get_export_address_table_entry(*export_addr_index, pe.base_address)
            .unwrap();
        let export_absolute = export_addr.0.get(pe.base_address) as *const _ as usize;
        let is_within_mod_range = {
            let export_addr_abs = export_addr as *const _ as usize;
            let mod_base = hmod.get_base_address();
            let mod_end = mod_base + hmod.get_size();
            export_addr_abs >= mod_base && export_addr_abs < mod_end
        };
        assert!(is_within_mod_range);
        let is_forwarder = pe
            .get_data_directories()
            .is_within_range(ImageDataDirectoryEntry::ExportTable, export_absolute)
            .unwrap();
        assert!(!is_forwarder);
    }

    /// Test get_proc_address to get CreateFileA from kernel32
    #[test]
    fn test_get_proc_address() {
        let hmod = get_module_by_name("kernel32.dll").unwrap();
        let res = hmod.parse_pe();
        assert!(res.is_ok(), "Failed to parse PE: {:#?}", res.err().unwrap());
        let enter_crit_sect = get_proc_address(hmod.as_ref(), "EnterCriticalSection");
        assert!(enter_crit_sect.is_err());
        let hmod2 = get_module_by_name("ntdll.dll").unwrap();
        let nt_create_file = get_proc_address(hmod2.as_ref(), "NtCreateFile");
        assert!(nt_create_file.is_ok());
    }
}

/// 64-bit LdrDataTableEntry
#[derive(Copy, Clone, Debug)]
#[repr(C)]
struct LdrDataTableEntry {
    in_load_order_links: ListEntry,
    in_memory_order_links: ListEntry,
    in_initialization_order_links: ListEntry,
    dll_base: usize,
    entry_point: usize,
    size_of_image: usize,
    full_dll_name: UnicodeString,
    base_dll_name: UnicodeString,
    flags: u32,
    load_count: u16,
    tls_index: u16,
    hash_links: ListEntry,
    time_date_stamp: u32,
}

/// Doubly linked list entry
#[derive(Copy, Clone, Debug)]
#[repr(C)]
struct ListEntry {
    flink: *const ListEntry,
    blink: *const ListEntry,
}

/// 64-bit PebLdrData
#[derive(Copy, Clone)]
#[repr(C)]
struct PebLdrData {
    junk: [usize; 4],
    in_memory_order_module_list: ListEntry,
}

/// Basic 64-bit PEB
#[derive(Copy, Clone)]
#[repr(C)]
struct PEB {
    junk1: u32,
    junk2: usize,
    junk3: usize,
    ldr: *const PebLdrData,
}
