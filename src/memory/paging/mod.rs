// paging module that reads and modifies the hierarchicak page table through recursive mapping

pub use self::entry::*;     //export for all entry types
pub use self::mapper::Mapper;
use core::ptr::Unique;
use memory::FrameAllocator;
use self::table::{Table, Level4};
use memory::PAGE_SIZE;
use memory::Frame;
use self::temporary_page::TemporaryPage;
use core::ops::{Deref, DerefMut};
use multiboot2::BootInformation;
use memory::paging::table::P4;

mod entry;
mod table;
mod temporary_page;
mod mapper;

const ENTRY_COUNT: usize = 512;     // number of entries per table

pub type PhysicalAddress = usize;
pub type VirtualAddress = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Page {
   number: usize,
}

impl Page {

    // get the Page from the virtual address
    pub fn containing_address(address: VirtualAddress) -> Page {
    // make sure we do not access a invalid virtual adress
    // address space is split up into two halves, one with sign extension adresses and one without
    // everything in between is invalid -> invalid address
    assert!(address < 0x0000_8000_0000_0000 ||
        address >= 0xffff_8000_0000_0000,
        "invalid address: 0x{:x}", address);
    Page { number: address / PAGE_SIZE }
    }

    fn start_address(&self) -> usize {
    self.number * PAGE_SIZE
    }

    // returns the different table indexes
    fn p4_index(&self) -> usize {
    (self.number >> 27) & 0o777
    }
    fn p3_index(&self) -> usize {
        (self.number >> 18) & 0o777
    }
    fn p2_index(&self) -> usize {
        (self.number >> 9) & 0o777
    }
    fn p1_index(&self) -> usize {
        (self.number >> 0) & 0o777
    }
    pub fn range_inclusive(start: Page, end: Page) -> PageIter {
    PageIter {
        start: start,
        end: end,
    }
  }
}

pub struct PageIter {
    start: Page,
    end: Page,
}

impl Iterator for PageIter {
    type Item = Page;

    fn next(&mut self) -> Option<Page> {
        if self.start <= self.end {
            let page = self.start;
            self.start.number += 1;
            Some(page)
        } else {
            None
        }
    }
}

// P4 table is owned by the ActivePageTable struct
// use unique to indicate ownership
pub struct ActivePageTable {
    mapper: Mapper,
}

//The Deref and DerefMut implementations allow us to use the ActivePageTable exactly as before
// closure in with function can no longer invoke with again
impl Deref for ActivePageTable {
    type Target = Mapper;

    fn deref(&self) -> &Mapper {
        &self.mapper
    }
}
impl DerefMut for ActivePageTable {
    fn deref_mut(&mut self) -> &mut Mapper {
        &mut self.mapper
    }
}

impl ActivePageTable {

    unsafe fn new() -> ActivePageTable {
        ActivePageTable {
            mapper: Mapper::new(),
        }
    }

    //temporary change the recursive mapping to point to the inactive P4 table
    pub fn with<F>(&mut self,
                   table: &mut InactivePageTable,
                   temporary_page: &mut temporary_page::TemporaryPage, // new
                   f: F)
               // fnonce allows captured variables to be moved out from the closure environment
               //closure gets a Mapper as argument instead of ActivePageTable
    where F: FnOnce(&mut Mapper)
    {
        use x86_64::instructions::tlb;
        use x86_64::registers::control_regs;

    {
        //create backup of the P4 entry by reading it from the CR3 control register
        //to restore it after the closure has run
        let backup = Frame::containing_address(
            control_regs::cr3().0 as usize);

        // map temporary_page to current p4 table
        let p4_table = temporary_page.map_table_frame(backup.clone(), self);

        // overwrite recursive mapping
        // overwrite P4 entry and point it to the inactive table frame
        self.p4_mut()[511].set(table.p4_frame.clone(), PRESENT | WRITABLE);

        //flush TLB so no old translations exist
        tlb::flush_all();

        // execute f in the new context when the recursive mapping now points to an inactive table
        f(self);

        // restore recursive mapping to original p4 table
        p4_table[511].set(backup, PRESENT | WRITABLE);
        tlb::flush_all();
    }

        temporary_page.unmap(self);
    }

    // switch tables
    // reload cr3 with the physical address of the new P4 frame
    pub fn switch(&mut self, new_table: InactivePageTable) -> InactivePageTable {
    use x86_64::PhysicalAddress;
    use x86_64::registers::control_regs;

    let old_table = InactivePageTable {
        p4_frame: Frame::containing_address(
            control_regs::cr3().0 as usize
        ),
    };
    unsafe {
        control_regs::cr3_write(PhysicalAddress(
            new_table.p4_frame.start_address() as u64));
    }
    old_table
}
}

// used on inactie page tables
// not used by CPU
pub struct InactivePageTable {
    p4_frame: Frame,
}

impl InactivePageTable {

    //to zero the table
    //we can now create valid inactive page tables
    pub fn new(frame: Frame, active_table: &mut ActivePageTable, temporary_page: &mut TemporaryPage) -> InactivePageTable
    {
        {   //map page to page table
            let table = temporary_page.map_table_frame(frame.clone(),
                active_table);

            // now we are able to zero the table
            table.zero();
            // set up recursive mapping for the table
            table[511].set(frame.clone(), PRESENT | WRITABLE);
        }
        temporary_page.unmap(active_table);

        InactivePageTable { p4_frame: frame }
    }
}

// map kernel sections in new page table
pub fn remap_the_kernel<A>(allocator: &mut A, boot_info: &BootInformation)
    -> ActivePageTable
    where A: FrameAllocator
{
    let mut temporary_page = TemporaryPage::new(Page { number: 0xcafebabe },
        allocator);

    let mut active_table = unsafe { ActivePageTable::new() };
    let mut new_table = {
        let frame = allocator.allocate_frame().expect("no more frames");
        InactivePageTable::new(frame, &mut active_table, &mut temporary_page)
    };

    active_table.with(&mut new_table, &mut temporary_page, |mapper| {
        let elf_sections_tag = boot_info.elf_sections_tag()
            .expect("Memory map tag required");

        //identity map the kernel sections
        for section in elf_sections_tag.sections() {

            use self::entry::WRITABLE;

            if !section.is_allocated() {
                // section is not loaded to memory
                continue;
            }
            assert!(section.start_address() % PAGE_SIZE == 0,
                    "sections need to be page aligned");

            println!("mapping section at addr: {:#x}, size: {:#x}",
                section.addr, section.size);

            let flags = EntryFlags::from_elf_section_flags(section);

            let start_frame = Frame::containing_address(section.start_address());
            let end_frame = Frame::containing_address(section.end_address() - 1);
            for frame in Frame::range_inclusive(start_frame, end_frame) {
                mapper.identity_map(frame, flags, allocator);
            }
        }

        // identity map the VGA text buffer
        let vga_buffer_frame = Frame::containing_address(0xb8000);
        mapper.identity_map(vga_buffer_frame, WRITABLE, allocator);

        // identity map the multiboot info structure
        let multiboot_start = Frame::containing_address(boot_info.start_address());
        let multiboot_end = Frame::containing_address(boot_info.end_address() - 1);
        for frame in Frame::range_inclusive(multiboot_start, multiboot_end) {
            mapper.identity_map(frame, PRESENT, allocator);
        }

    });

    let old_table = active_table.switch(new_table);
    println!("NEW TABLE!!!");

    // turn the old p4 page into a guard page
    let old_p4_page = Page::containing_address(
      old_table.p4_frame.start_address()
    );
    active_table.unmap(old_p4_page, allocator);
    println!("guard page at {:#x}", old_p4_page.start_address());

    active_table
}

// function to test the paging
pub fn test_paging<A>(allocator: &mut A)
    where A: FrameAllocator
{
    let mut page_table = unsafe { ActivePageTable::new() };

    let addr = 42 * 512 * 512 * 4096; // 42th P3 entry
    let page = Page::containing_address(addr);
    let frame = allocator.allocate_frame().expect("no more frames");

    println!("None = {:?}, map to {:?}", page_table.translate(addr),frame);

    page_table.map_to(page, frame, EntryFlags::empty(), allocator);

    println!("Some = {:?}", page_table.translate(addr));
    println!("next free frame: {:?}", allocator.allocate_frame());

    page_table.unmap(Page::containing_address(addr), allocator);
    println!("None = {:?}", page_table.translate(addr));

    println!("{:#x}", unsafe {
    *(Page::containing_address(addr).start_address() as *const u64)
    });
}
