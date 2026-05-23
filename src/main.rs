//! TablesOS image builder / runner.
//!
//! No third-party bootloader, no partitions, no FAT (SPECIFICATION.md item 8).
//! We assemble our own stage 1 (custom MBR) and stage 2, flatten the kernel
//! ELF to a raw binary, and lay everything onto **one** disk image:
//!
//! ```text
//! LBA 0      stage 1 / custom MBR (512 B, header patched here)
//! LBA 1..    stage 2
//! LBA 64..   kernel (flat)
//! LBA <data> TablesOS volume (rest of the device)
//! ```
//!
//! `cargo run` builds the image and boots it as the single disk in QEMU.

use std::path::{Path, PathBuf};
use std::process::Command;

const KERNEL_ELF: &str = env!("CARGO_BIN_FILE_KERNEL_kernel");

const SECTOR: usize = 512;
const STAGE2_LBA: u32 = 1;
const STAGE2_MAX_SECTORS: u32 = 63; // stage1 reads a fixed 63 sectors
const KERNEL_LBA: u32 = 64;
const KERNEL_LOAD: u32 = 0x0020_0000;
const IMG_SECTORS: u64 = 64 * 1024 * 1024 / SECTOR as u64; // 64 MiB device

// Custom MBR header field offsets (see boot/layout.md / boot/stage1.s).
const H_DATA_LBA: usize = 0x1BC;
const H_S2_LBA: usize = 0x1C4;
const H_S2_SECS: usize = 0x1C8;
const H_K_LBA: usize = 0x1CC;
const H_K_SECS: usize = 0x1D0;
const H_K_LOAD: usize = 0x1D4;
const H_K_ENTRY: usize = 0x1D8;
const H_SYS_GUID: usize = 0x1DC; // 16-byte unique system id

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let boot = manifest.join("boot");
    let out = manifest.join("target");

    let as_exe = tool("AS", "as");
    // GNU objcopy handles COFF→binary correctly (llvm-objcopy mis-sizes COFF);
    // llvm-objcopy is needed for the ELF kernel (GNU mingw objcopy can't read
    // ELF). So: GNU for the stages, LLVM for the kernel.
    let gnu_objcopy = tool("OBJCOPY", "objcopy");
    let llvm_oc = llvm_objcopy();

    // 1. Assemble the two boot stages to flat binaries (relocation-free asm,
    //    no link step — see boot/stage1.s).
    let stage1 = assemble(&as_exe, &gnu_objcopy, &boot, &out, "stage1");
    let stage2 = assemble(&as_exe, &gnu_objcopy, &boot, &out, "stage2");

    // 2. Flatten the kernel ELF (drops NOLOAD .bss; entry == load address).
    let kernel_bin = out.join("kernel.bin");
    run(Command::new(&llvm_oc).args([
        "-O",
        "binary",
        KERNEL_ELF,
        kernel_bin.to_str().unwrap(),
    ]));
    let kernel = std::fs::read(&kernel_bin).expect("read kernel.bin");

    // 3. Lay out the single image.
    assert_eq!(stage1.len(), SECTOR, "stage1 must be exactly one sector");
    let s2_secs = sectors(stage2.len());
    assert!(
        s2_secs <= STAGE2_MAX_SECTORS,
        "stage2 is {s2_secs} sectors, max {STAGE2_MAX_SECTORS}"
    );
    let k_secs = sectors(kernel.len());
    let data_lba = align_up(KERNEL_LBA as u64 + k_secs as u64, 2048); // 1 MiB
    assert!(
        data_lba + 4096 < IMG_SECTORS,
        "kernel too large; volume would be empty"
    );

    let mut img = vec![0u8; IMG_SECTORS as usize * SECTOR];
    img[..SECTOR].copy_from_slice(&stage1);
    put_u64(&mut img, H_DATA_LBA, data_lba);
    put_u32(&mut img, H_S2_LBA, STAGE2_LBA);
    put_u16(&mut img, H_S2_SECS, s2_secs as u16);
    put_u32(&mut img, H_K_LBA, KERNEL_LBA);
    put_u32(&mut img, H_K_SECS, k_secs);
    put_u32(&mut img, H_K_LOAD, KERNEL_LOAD);
    put_u32(&mut img, H_K_ENTRY, KERNEL_LOAD);

    // Unique system GUID for this image. The kernel re-reads it from the MBR
    // and refuses to run on any disk whose GUID differs from the one it
    // booted with — so it can never touch a disk that isn't this one.
    let guid = random_guid();
    img[H_SYS_GUID..H_SYS_GUID + 16].copy_from_slice(&guid);

    let s2_off = STAGE2_LBA as usize * SECTOR;
    img[s2_off..s2_off + stage2.len()].copy_from_slice(&stage2);
    let k_off = KERNEL_LBA as usize * SECTOR;
    img[k_off..k_off + kernel.len()].copy_from_slice(&kernel);

    // Lay down an ALREADY-FORMATTED TablesOS volume (the OS never formats at
    // runtime — see SPECIFICATION.md). We format an in-memory volume of the
    // exact size the kernel will see (whole device − data_lba) and copy it in.
    let vol_sectors = IMG_SECTORS - data_lba;
    let mut st = tablestore::Store::format(tablestore::MemBlockDevice::new(vol_sectors))
        .expect("format in-memory volume");
    let vol = st.device_mut().snapshot();
    let voff = data_lba as usize * SECTOR;
    assert_eq!(vol.len(), img.len() - voff, "volume size mismatch");
    img[voff..].copy_from_slice(&vol);

    let image = out.join("tablesos.img");
    std::fs::write(&image, &img).expect("write image");
    println!(
        "single-device image: {} ({} MiB)\n  stage2 = {s2_secs} sectors, kernel = {k_secs} sectors, volume @ LBA {data_lba}",
        image.display(),
        img.len() / 1024 / 1024
    );
    println!(
        "Flash to a pendrive (whole device):  dd if={} of=/dev/sdX bs=4M conv=fsync",
        image.display()
    );

    if std::env::args().any(|a| a == "--no-run") {
        return;
    }

    // An empty backing file for the emulated USB mass-storage stick we
    // attach to the xHCI controller. Avoid `usb-kbd` — QEMU routes the
    // host keyboard to the USB device when one is present, which leaves
    // the PS/2 driver silent. `usb-storage` is what the USB stack will
    // ultimately drive anyway, so it's the right thing to ship here.
    //
    // Sized to match the main image (64 MiB) so the "install TablesOS on
    // USB" flow has room for the replicated ~3-4 MiB boot prefix *and* a
    // formattable volume (the TablesOS volume needs ~8 MiB minimum just
    // for its journal region). A previously-created 4 MiB stub is grown.
    let usb_img = out.join("usbstick.img");
    const USB_IMG_BYTES: u64 = 64 * 1024 * 1024;
    let need_create = match std::fs::metadata(&usb_img) {
        Ok(m) => m.len() < USB_IMG_BYTES,
        Err(_) => true,
    };
    if need_create {
        let blank = vec![0u8; USB_IMG_BYTES as usize];
        std::fs::write(&usb_img, &blank).expect("create usbstick.img");
        eprintln!(
            "created {} MiB USB backing image at {}",
            USB_IMG_BYTES >> 20,
            usb_img.display()
        );
    }

    let qemu = find_qemu();
    let status = Command::new(&qemu)
        .args(["-machine", "pc"])
        // TablesOS owns the whole machine and now keeps a 256 MiB heap (five
        // cached full-screen background composites + the engine working set),
        // so give the VM generous RAM. On real hardware the kernel uses only
        // its fixed .bss heap regardless of installed RAM.
        .args(["-m", "1024M"])
        .args([
            "-drive",
            &format!("format=raw,file={},if=ide,index=0,media=disk", image.display()),
        ])
        // Expose an xHCI controller (id=xhci) so we can attach USB devices
        // to its bus specifically. Attach a USB mass-storage device backed
        // by `usbstick.img` — it has Bulk IN + Bulk OUT endpoints (which
        // the future USB-MSC code will drive) and, crucially, does *not*
        // grab the host keyboard, so PS/2 input still works.
        .args(["-device", "qemu-xhci,id=xhci"])
        .args([
            "-drive",
            &format!(
                "if=none,id=usbstick,format=raw,file={}",
                usb_img.display()
            ),
        ])
        .args(["-device", "usb-storage,bus=xhci.0,drive=usbstick"])
        .args(["-serial", "stdio"])
        .args(["-vga", "std"])
        .arg("-no-reboot")
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("could not launch QEMU ({}): {e}", qemu.display());
            eprintln!(
                "Set the QEMU env var to qemu-system-x86_64. Image is at {}",
                image.display()
            );
        }
    }
}

// ---- helpers -------------------------------------------------------------

/// A unique 16-byte system id. Not cryptographic — just unique per build,
/// which is all the boot-disk identity check needs. Mixes wall-clock,
/// process id and a stack address through splitmix64.
fn random_guid() -> [u8; 16] {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let local = 0u8;
    let mut s = t
        ^ ((std::process::id() as u64) << 32)
        ^ (&local as *const u8 as u64);
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut g = [0u8; 16];
    g[0..8].copy_from_slice(&next().to_le_bytes());
    g[8..16].copy_from_slice(&next().to_le_bytes());
    g
}

fn sectors(len: usize) -> u32 {
    ((len + SECTOR - 1) / SECTOR) as u32
}
fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) / a * a
}
fn put_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(status.success(), "command failed: {cmd:?}");
}

/// Assemble `boot/<name>.s` to a flat binary: `as` → object, then
/// `objcopy -O binary -j .boot` (the asm is relocation-free, so no linker).
fn assemble(as_exe: &Path, objcopy: &Path, boot: &Path, out: &Path, name: &str) -> Vec<u8> {
    let src = boot.join(format!("{name}.s"));
    let obj = out.join(format!("{name}.o"));
    let bin = out.join(format!("{name}.bin"));
    run(Command::new(as_exe).args([
        "-o",
        obj.to_str().unwrap(),
        src.to_str().unwrap(),
    ]));
    run(Command::new(objcopy).args([
        "-O",
        "binary",
        "-j",
        ".boot",
        obj.to_str().unwrap(),
        bin.to_str().unwrap(),
    ]));
    std::fs::read(&bin).unwrap_or_else(|e| panic!("read {}: {e}", bin.display()))
}

/// Resolve a GNU tool: `$ENV` → MSYS2 mingw64 → PATH.
fn tool(env: &str, exe: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env) {
        return PathBuf::from(p);
    }
    let win = format!(r"C:\msys64\mingw64\bin\{exe}.exe");
    if Path::new(&win).exists() {
        return PathBuf::from(win);
    }
    PathBuf::from(exe) // fall back to PATH
}

/// `llvm-objcopy` ships with the Rust toolchain (`llvm-tools` component).
fn llvm_objcopy() -> PathBuf {
    if let Some(p) = std::env::var_os("OBJCOPY") {
        return PathBuf::from(p);
    }
    if let Ok(o) = Command::new("rustc").args(["--print", "sysroot"]).output() {
        let sysroot = String::from_utf8_lossy(&o.stdout).trim().to_string();
        for host in ["x86_64-pc-windows-gnu", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"] {
            for name in ["llvm-objcopy.exe", "llvm-objcopy"] {
                let p = PathBuf::from(&sysroot)
                    .join("lib/rustlib")
                    .join(host)
                    .join("bin")
                    .join(name);
                if p.exists() {
                    return p;
                }
            }
        }
    }
    // Fall back to a GNU objcopy.
    tool("OBJCOPY", "objcopy")
}

fn find_qemu() -> PathBuf {
    if let Some(p) = std::env::var_os("QEMU") {
        return PathBuf::from(p);
    }
    let exe = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };
    let mut c: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for b in [
            r"C:\Program Files\qemu",
            r"C:\Program Files (x86)\qemu",
            r"C:\msys64\mingw64\bin",
        ] {
            c.push(PathBuf::from(b).join(exe));
        }
    } else {
        for b in ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            c.push(PathBuf::from(b).join(exe));
        }
    }
    c.into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe))
}
