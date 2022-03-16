// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines functionality for creating guest memory snapshots.

// Currently only used on x86_64.
#![cfg(target_arch = "x86_64")]

use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::SeekFrom;
use std::io;
use std::collections::HashMap;
use std::ptr::null_mut;
use std::thread;

use libc::printf;
use logger::info;
// for userfaultfd
use std::path::PathBuf;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use userfaultfd::UffdBuilder;
use passfd::FdPassingExt;

use versionize::{VersionMap, Versionize, VersionizeResult};
use versionize_derive::Versionize;
use vm_memory::{Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap, MemoryRegionAddress, MmapRegion, mmap};

use crate::DirtyBitmap;

/// State of a guest memory region saved to file/buffer.
#[derive(Debug, PartialEq, Versionize)]
pub struct GuestMemoryRegionState {
    /// Base address.
    pub base_address: u64,
    /// Region size.
    pub size: usize,
    /// Offset in file/buffer where the region is saved.
    pub offset: u64,
}

/// Guest memory state.
#[derive(Debug, Default, PartialEq, Versionize)]
pub struct GuestMemoryState {
    /// List of regions.
    pub regions: Vec<GuestMemoryRegionState>,
}

/// Defines the interface for snapshotting memory.
pub trait SnapshotMemory
where
    Self: Sized,
{
    /// Describes GuestMemoryMmap through a GuestMemoryState struct.
    fn describe(&self) -> GuestMemoryState;
    /// Dumps all contents of GuestMemoryMmap to a writer.
    fn dump<T: std::io::Write>(&self, writer: &mut T) -> std::result::Result<(), Error>;
    /// Dumps all pages of GuestMemoryMmap present in `dirty_bitmap` to a writer.
    fn dump_dirty<T: std::io::Write + std::io::Seek>(
        &self,
        writer: &mut T,
        dirty_bitmap: &DirtyBitmap,
    ) -> std::result::Result<(), Error>;
    /// Creates a GuestMemoryMmap given a `file` containing the data
    /// and a `state` containing mapping information.
    fn restore(mem_file_path: &PathBuf,
        mem_state: &GuestMemoryState,
        enable_user_page_faults: bool,
        overlay_file_path: &PathBuf,
        overlay_regions: &HashMap<i64, i64>,
        ws_file_path: &PathBuf,
        ws_regions: &Vec<Vec<i64>>,
        load_ws: bool,
        fadvise: &String,
    ) -> std::result::Result<Self, Error>;
    /// Registers guest memory for hanlding page faults with an external user-level process
    fn register_for_upf(&self, sock_file_path: &PathBuf) -> std::result::Result<(), Error>;
    /// load working set
    fn load_working_set(&self, ws_regions: &Vec<Vec<i64>>) -> std::result::Result<(), Error>;
}

/// Errors associated with dumping guest memory to file.
#[derive(Debug)]
pub enum Error {
    /// Cannot access file.
    FileHandle(std::io::Error),
    /// Cannot create memory.
    CreateMemory(vm_memory::Error),
    /// Cannot create region.
    CreateRegion(vm_memory::mmap::MmapRegionError),
    /// Cannot dump memory.
    WriteMemory(GuestMemoryError),
    /// Cannot register region for user page fault handling.
    UserPageFault(userfaultfd::Error),
    /// Overlay regions error.
    OverlayRegions(std::io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;
        match self {
            FileHandle(err) => write!(f, "Cannot access file: {:?}", err),
            CreateMemory(err) => write!(f, "Cannot create memory: {:?}", err),
            CreateRegion(err) => write!(f, "Cannot create memory region: {:?}", err),
            WriteMemory(err) => write!(f, "Cannot dump memory: {:?}", err),
            UserPageFault(err) => write!(f, "Cannot register memory for uPF: {:?}", err),
            OverlayRegions(err) => write!(f, "Cannot mmap overlay regions: {:?}", err),            
        }
    }
}

impl SnapshotMemory for GuestMemoryMmap {
    /// Describes GuestMemoryMmap through a GuestMemoryState struct.
    fn describe(&self) -> GuestMemoryState {
        let mut guest_memory_state = GuestMemoryState::default();
        let mut offset = 0;
        let _: std::result::Result<(), ()> = self.with_regions_mut(|_, region| {
            guest_memory_state.regions.push(GuestMemoryRegionState {
                base_address: region.start_addr().0,
                size: region.len() as usize,
                offset,
            });

            offset += region.len();
            Ok(())
        });
        guest_memory_state
    }

    /// Dumps all contents of GuestMemoryMmap to a writer.
    fn dump<T: std::io::Write>(&self, writer: &mut T) -> std::result::Result<(), Error> {
        self.with_regions_mut(|_, region| {
            region.write_all_to(MemoryRegionAddress(0), writer, region.len() as usize)
        })
        .map_err(Error::WriteMemory)
    }

    /// Dumps all pages of GuestMemoryMmap present in `dirty_bitmap` to a writer.
    fn dump_dirty<T: std::io::Write + std::io::Seek>(
        &self,
        writer: &mut T,
        dirty_bitmap: &DirtyBitmap,
    ) -> std::result::Result<(), Error> {
        let page_size = sysconf::page::pagesize();
        let mut writer_offset = 0;

        self.with_regions_mut(|slot, region| {
            let bitmap = dirty_bitmap.get(&slot).unwrap();
            let mut write_size = 0;
            let mut dirty_batch_start: u64 = 0;

            for (i, v) in bitmap.iter().enumerate() {
                for j in 0..64 {
                    let is_dirty_page = ((v >> j) & 1u64) != 0u64;
                    if is_dirty_page {
                        let page_offset = ((i * 64) + j) * page_size;
                        // We are at the start of a new batch of dirty pages.
                        if write_size == 0 {
                            // Seek forward over the unmodified pages.
                            writer
                                .seek(SeekFrom::Start(writer_offset + page_offset as u64))
                                .unwrap();
                            dirty_batch_start = page_offset as u64;
                        }
                        write_size += page_size;
                    } else if write_size > 0 {
                        // We are at the end of a batch of dirty pages.
                        region.write_all_to(
                            MemoryRegionAddress(dirty_batch_start),
                            writer,
                            write_size,
                        )?;
                        write_size = 0;
                    }
                }
            }

            if write_size > 0 {
                region.write_all_to(MemoryRegionAddress(dirty_batch_start), writer, write_size)?;
            }

            writer_offset += region.len();
            Ok(())
        })
        .map_err(Error::WriteMemory)
    }

    /// Creates a GuestMemoryMmap given a `file` containing the data
    /// and a `state` containing mapping information.
    fn restore(mem_file_path: &PathBuf,
        state: &GuestMemoryState,
        enable_user_page_faults: bool,
        overlay_file_path: &PathBuf,
        overlay_regions: &HashMap<i64, i64>,
        ws_file_path: &PathBuf,
        ws_regions: &Vec<Vec<i64>>,
        load_ws: bool,
        fadvise: &String,
    ) -> std::result::Result<Self, Error> {
        let page_size = sysconf::page::pagesize() as i64;
        let mut mmap_regions = Vec::new();
        assert!(state.regions.len() == 1); // for now only support one region
        for region in state.regions.iter() {
            assert!(region.offset == 0);

            let (flags, file_offset) = if mem_file_path.clone().into_os_string().eq("") { // no memfile, anony mapping
                (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, None)
            } else { // backing file
                let file = File::open(mem_file_path).map_err(Error::FileHandle)?;
                (libc::MAP_NORESERVE | libc::MAP_PRIVATE, Some(FileOffset::new(
                    file.try_clone().map_err(Error::FileHandle)?,
                    region.offset,
                )))
            };

            let mmap_region = MmapRegion::build( // build base layer
                file_offset,
                region.size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
            )
            .map(|r| GuestRegionMmap::new(r, GuestAddress(region.base_address)))
            .map_err(Error::CreateRegion)?
            .map_err(Error::CreateMemory)?;
            info!("base layer mmap'd. offset = {:?}, len={:?}", region.offset, region.size);
            let addr = mmap_region.as_ptr();
            // overlay layer
            if !overlay_file_path.clone().into_os_string().eq("") {
                let file = File::open(overlay_file_path).map_err(Error::FileHandle)?;
                let fd = file.as_raw_fd();
                for (off, len) in overlay_regions {
                    let offset = *off * page_size;
                    let length = *len * page_size;
                    let ret = unsafe { libc::mmap((addr.offset(offset as isize)) as *mut u8 as _, length as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_FIXED | libc::MAP_NORESERVE | libc::MAP_PRIVATE, fd, offset as libc::off_t)};
                    if ret == libc::MAP_FAILED {
                        return Err(Error::OverlayRegions(std::io::Error::last_os_error()));
                    }
                }
            }

            // working set layer
            if !ws_file_path.clone().into_os_string().eq("") {
                let file = File::open(ws_file_path).map_err(Error::FileHandle)?;
                let fd = file.as_raw_fd();
                let mut file_off: u64 = 0;
                for region in ws_regions {
                    let off = region[0] * page_size;
                    let len = region[1] * page_size;
                    let fd = file.as_raw_fd();
                    let ret = unsafe { libc::mmap((addr.offset(off as isize)) as *mut u8 as _, len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_FIXED | libc::MAP_NORESERVE | libc::MAP_PRIVATE, fd, (file_off) as libc::off_t) };
                    if ret == libc::MAP_FAILED {
                        return Err(Error::OverlayRegions(std::io::Error::last_os_error()));
                    }
                    file_off += len as u64;
                }
            }
            mmap_regions.push(mmap_region);
        }
    
        // if load_ws {
        //         let start = addr.clone() as u64;
        //         let new_ws_regions = ws_regions.clone();
        //         let new_ol_regions = overlay_regions.clone();
        //         thread::Builder::new()
        //             .name("fc_ws_loader".to_owned()).spawn(move || {
        //             info!("in the thread");
        //             let mut a: u8 = 0;
        //             let mut sorted: Vec<_> = new_ws_regions.into_iter().collect();
        //             sorted.sort_by(|x,y| x.1.cmp(&y.1));
        //             for (off, file_off) in sorted {
        //                 let len = new_ol_regions[&off];
        //                 for pos in (off..off+len).step_by(4096) {
        //                     unsafe {a ^= *((start as *const u8).offset(pos as isize))};
        //                 }
        //             }
        //             info!("loaded, {}", a);
        //         }).expect("loader thread spawn failed.");
        //     }

        Ok(Self::from_regions(mmap_regions).map_err(Error::CreateMemory)?)
    }

    /// Use both memfile and wsfile
    // fn restore2(memfile: &File, wsfile: &File, state: &GuestMemoryState, enable_user_page_faults: bool, overlay_regions: &HashMap<i64, i64>, groups: &Vec<Vec<i64>>, load_ws: bool) -> std::result::Result<Self, Error> {
    //     let page_size = sysconf::page::pagesize() as i64;
    //     let mut mmap_regions = Vec::new();
    //     assert!(state.regions.len() == 1); // for now only support one region
    //     for region in state.regions.iter() {
    //         assert!(region.offset == 0);
    //         // anonymous memory as base layer
    //         let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    //         let mmap_region = MmapRegion::build(
    //             None,
    //             region.size,
    //             libc::PROT_READ | libc::PROT_WRITE,
    //             flags,
    //         )
    //         .map(|r| GuestRegionMmap::new(r, GuestAddress(region.base_address)))
    //         .map_err(Error::CreateRegion)?
    //         .map_err(Error::CreateMemory)?;
    //         info!("original region's offset = {:?}, len={:?}", region.offset, region.size);
    //         let addr = mmap_region.as_ptr();
    //         // memfile as non-active upper layer
    //         for (off, len) in overlay_regions {
    //             let file_off = *off as u64;
    //             let fd = memfile.as_raw_fd();
    //             let ret = unsafe { libc::mmap((addr.offset(*off as isize)) as *mut u8 as _, *len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_FIXED | libc::MAP_NORESERVE | libc::MAP_PRIVATE, fd, (file_off) as libc::off_t) };
    //             if ret == libc::MAP_FAILED {
    //                 return Err(Error::OverlayRegions(std::io::Error::last_os_error()));
    //             }
    //         }
    //         // wsfile as active upper layer
    //         let mut file_off: u64 = 0;
    //         for group in groups {
    //             let off = group[0] * page_size;
    //             let len = group[1] * page_size;
    //             let fd = wsfile.as_raw_fd();
    //             let ret = unsafe { libc::mmap((addr.offset(off as isize)) as *mut u8 as _, len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_FIXED | libc::MAP_NORESERVE | libc::MAP_PRIVATE, fd, (file_off) as libc::off_t) };
    //             if ret == libc::MAP_FAILED {
    //                 return Err(Error::OverlayRegions(std::io::Error::last_os_error()));
    //             }
    //             file_off += len as u64;
    //         }
    //         mmap_regions.push(mmap_region);
    //     }
    //     Ok(Self::from_regions(mmap_regions).map_err(Error::CreateMemory)?)
    // }    

    /// Registers guest memory regions for handling page faults
    /// with an external user-level process.
    fn register_for_upf(&self, sock_file_path: &PathBuf) -> std::result::Result<(), Error> {
        self.with_regions(|_, region| {
            info!("Guest memory size={:?}MB, base_address={:?}, last_addr={:?}",
                region.len()/1024/1024,
                region.get_host_address(region.to_region_addr(region.start_addr()).unwrap()),
                region.get_host_address(region.to_region_addr(region.last_addr()).unwrap()));

            let uffd = UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(true)
            .create()
            .expect("uffd creation");

            let addr = region.get_host_address(region.to_region_addr(region.start_addr()).unwrap()).unwrap();
            let len = region.len();
            info!("Host address of the region's start = {:p}, len={:?}", addr, len);
            uffd.register(addr as *mut u8 as _, len as u64 as _).expect("uffd.register()");

            let listener = UnixListener::bind(sock_file_path).unwrap();
            let (stream, _) = listener.accept().unwrap();
            stream.send_fd(uffd.as_raw_fd()).unwrap();

            info!("Sent the fd!");

            // Cause a page fault on the first page to communicate the start_addr's hVA
            unsafe{
                print!("after reg: ptr={:p}, mem value = {:?}, len={:?}", addr, *addr, len)
            }

            Ok(())
        })
        .map_err(Error::UserPageFault)
    }

    fn load_working_set(&self, ws_regions: &Vec<Vec<i64>>) -> std::result::Result<(), Error> {
        self.with_regions(|_, region| {
            info!("Start loading working set");

            let addr = region.get_host_address(region.to_region_addr(region.start_addr()).unwrap()).unwrap();
            let len = region.len();
            info!("Host address of the region's start = {:p}, len={:?}", addr, len);
            // let mut sorted: Vec<_> = ws_regions.into_iter().collect();
            // sorted.sort_by(|x,y| x.1.cmp(&y.1));
            let mut a: u8 = 0;
            let page_size = sysconf::page::pagesize() as i64;
            for item in ws_regions {
                let off = item[0] * page_size;
                let len = item[1] * page_size;
                for pos in (off..off+len).step_by(page_size as usize) {
                    unsafe {a ^= *((addr as *const u8).offset(pos as isize))};
                }
            }
            info!("loaded, {}", a);
            Ok(())
        })
        .map_err(Error::FileHandle)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use utils::tempfile::TempFile;
    use vm_memory::GuestAddress;

    #[test]
    fn test_describe_state() {
        let page_size: usize = sysconf::page::pagesize();

        // Two regions of one page each, with a one page gap between them.
        let mem_regions = [
            (GuestAddress(0), page_size),
            (GuestAddress(page_size as u64 * 2), page_size),
        ];
        let guest_memory = GuestMemoryMmap::from_ranges(&mem_regions[..]).unwrap();

        let expected_memory_state = GuestMemoryState {
            regions: vec![
                GuestMemoryRegionState {
                    base_address: 0,
                    size: page_size,
                    offset: 0,
                },
                GuestMemoryRegionState {
                    base_address: page_size as u64 * 2,
                    size: page_size,
                    offset: page_size as u64,
                },
            ],
        };

        let actual_memory_state = guest_memory.describe();
        assert_eq!(expected_memory_state, actual_memory_state);

        // Two regions of three pages each, with a one page gap between them.
        let mem_regions = [
            (GuestAddress(0), page_size * 3),
            (GuestAddress(page_size as u64 * 4), page_size * 3),
        ];
        let guest_memory = GuestMemoryMmap::from_ranges(&mem_regions[..]).unwrap();

        let expected_memory_state = GuestMemoryState {
            regions: vec![
                GuestMemoryRegionState {
                    base_address: 0,
                    size: page_size * 3,
                    offset: 0,
                },
                GuestMemoryRegionState {
                    base_address: page_size as u64 * 4,
                    size: page_size * 3,
                    offset: page_size as u64 * 3,
                },
            ],
        };

        let actual_memory_state = guest_memory.describe();
        assert_eq!(expected_memory_state, actual_memory_state);
    }

    #[test]
    fn test_restore_memory() {
        let page_size: usize = sysconf::page::pagesize();

        // Two regions of two pages each, with a one page gap between them.
        let mem_regions = [
            (GuestAddress(0), page_size * 2),
            (GuestAddress(page_size as u64 * 3), page_size * 2),
        ];
        let guest_memory = GuestMemoryMmap::from_ranges(&mem_regions[..]).unwrap();

        // Fill the first region with 1s and the second with 2s.
        let first_region = vec![1u8; page_size * 2];
        guest_memory
            .write(&first_region[..], GuestAddress(0))
            .unwrap();

        let second_region = vec![2u8; page_size * 2];
        guest_memory
            .write(&second_region[..], GuestAddress(page_size as u64 * 3))
            .unwrap();

        let memory_state = guest_memory.describe();

        // Case 1: dump the full memory.
        {
            let memory_file = TempFile::new().unwrap();
            guest_memory.dump(&mut memory_file.as_file()).unwrap();

            let restored_guest_memory =
                GuestMemoryMmap::restore(&memory_file.as_file(), &memory_state).unwrap();

            // Check that the region contents are the same.
            let mut actual_region = vec![0u8; page_size * 2];
            restored_guest_memory
                .read(&mut actual_region.as_mut_slice(), GuestAddress(0))
                .unwrap();
            assert_eq!(first_region, actual_region);

            restored_guest_memory
                .read(
                    &mut actual_region.as_mut_slice(),
                    GuestAddress(page_size as u64 * 3),
                )
                .unwrap();
            assert_eq!(second_region, actual_region);
        }

        // Case 2: dump only the dirty pages.
        {
            // First region pages: [dirty, clean]
            // Second region pages: [clean, dirty]
            let mut dirty_bitmap: DirtyBitmap = HashMap::new();
            dirty_bitmap.insert(0, vec![0b01; 1]);
            dirty_bitmap.insert(1, vec![0b10; 1]);

            let file = TempFile::new().unwrap();
            guest_memory
                .dump_dirty(&mut file.as_file(), &dirty_bitmap)
                .unwrap();

            let restored_guest_memory =
                GuestMemoryMmap::restore(&file.as_file(), &memory_state).unwrap();

            // Check that only the dirty pages have been restored.
            let zeros = vec![0u8; page_size];
            let ones = vec![1u8; page_size];
            let twos = vec![2u8; page_size];
            let expected_first_region = [ones.as_slice(), zeros.as_slice()].concat();
            let expected_second_region = [zeros.as_slice(), twos.as_slice()].concat();

            let mut actual_region = vec![0u8; page_size * 2];
            restored_guest_memory
                .read(&mut actual_region.as_mut_slice(), GuestAddress(0))
                .unwrap();
            assert_eq!(expected_first_region, actual_region);

            restored_guest_memory
                .read(
                    &mut actual_region.as_mut_slice(),
                    GuestAddress(page_size as u64 * 3),
                )
                .unwrap();
            assert_eq!(expected_second_region, actual_region);
        }
    }
}
