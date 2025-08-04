use libc::{mmap, munmap, msync, MAP_FAILED, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE};
use rayon::prelude::*;
use std::fs::{create_dir_all, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;

const FILE_COUNT: usize = 10_000;
const MODULES_DIR: &str = "modules";
const CREATE_CONTENT: &[u8] = b"Files Created";
const UPDATE_CONTENT: &[u8] = b"Files Updated";

fn create_file(file_idx: usize) -> io::Result<()> {
    let file_path = format!("{}/file_{}.txt", MODULES_DIR, file_idx);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(&file_path)?;
    file.set_len(CREATE_CONTENT.len() as u64)?;
    let mmap = unsafe {
        let ptr = mmap(
            ptr::null_mut(),
            CREATE_CONTENT.len(),
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        if ptr == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        ptr
    };
    unsafe {
        ptr::copy_nonoverlapping(CREATE_CONTENT.as_ptr(), mmap as *mut u8, CREATE_CONTENT.len());
        if msync(mmap, CREATE_CONTENT.len(), MS_SYNC) != 0 {
            return Err(io::Error::last_os_error());
        }
        if munmap(mmap, CREATE_CONTENT.len()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn update_file(file_idx: usize) -> io::Result<()> {
    let file_path = format!("{}/file_{}.txt", MODULES_DIR, file_idx);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file_path)?;
    file.set_len(UPDATE_CONTENT.len() as u64)?;
    let mmap = unsafe {
        let ptr = mmap(
            ptr::null_mut(),
            UPDATE_CONTENT.len(),
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        if ptr == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        ptr
    };
    unsafe {
        ptr::copy_nonoverlapping(UPDATE_CONTENT.as_ptr(), mmap as *mut u8, UPDATE_CONTENT.len());
        if msync(mmap, UPDATE_CONTENT.len(), MS_SYNC) != 0 {
            return Err(io::Error::last_os_error());
        }
        if munmap(mmap, UPDATE_CONTENT.len()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    // Create modules directory
    create_dir_all(MODULES_DIR)?;

    // Create 10,000 files in parallel
    println!("Creating {} files...", FILE_COUNT);
    (0..FILE_COUNT)
        .into_par_iter()
        .try_for_each(create_file)?;

    // Update 10,000 files in parallel
    println!("Updating {} files...", FILE_COUNT);
    (0..FILE_COUNT)
        .into_par_iter()
        .try_for_each(update_file)?;

    println!("Done!");
    Ok(())
}