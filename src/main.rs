//! TablesOS image builder / runner.
//!
//! No third-party bootloader. We assemble our own stage 1 (custom MBR) and
//! stage 2 for the BIOS path, compile our own UEFI loader (BOOTX64.EFI) for
//! the UEFI path, flatten the kernel ELF to a raw binary, and lay everything
//! onto **one** hybrid disk image that boots both ways:
//!
//! ```text
//! LBA 0      stage 1 / custom MBR (512 B): boot code, TBLSBOOT header @0x180,
//!            partition table @0x1BE with one type-0xEF entry (the ESP)
//! LBA 1..    stage 2                                    (BIOS path)
//! LBA 64..   kernel (flat)
//! LBA <esp>  FAT16 EFI System Partition with \EFI\BOOT\BOOTX64.EFI (UEFI path)
//! LBA <data> TablesOS volume (rest of the device)
//! ```
//!
//! `cargo run` builds the image and boots it as the single disk in QEMU
//! (legacy BIOS); `cargo run -- --uefi` boots it under OVMF/EDK2 instead.

use std::path::{Path, PathBuf};
use std::process::Command;

mod fat;
mod seed;

const KERNEL_ELF: &str = env!("CARGO_BIN_FILE_KERNEL_kernel");
const UEFI_LOADER: &str = env!("CARGO_BIN_FILE_UEFI_LOADER");

const SECTOR: usize = 512;
const STAGE2_LBA: u32 = 1;
const STAGE2_MAX_SECTORS: u32 = 63; // stage1 reads a fixed 63 sectors
const KERNEL_LBA: u32 = 64;
// 16 MiB, not 2 MiB: the kernel footprint (image + .bss: a small fallback heap
// + stack) must clear the low-RAM islands UEFI firmware needs alive after boot
// (OVMF keeps ACPI NVS at 8 MiB). The kernel's real heap is no longer in .bss —
// each bootloader reserves a free block and reports it in BootInfo (see
// boot/layout.md), so the footprint here is small. Must match kernel/linker.ld.
const KERNEL_LOAD: u32 = 0x0100_0000;
const IMG_SECTORS: u64 = 64 * 1024 * 1024 / SECTOR as u64; // 64 MiB device

// Custom MBR header field offsets (see boot/layout.md / boot/stage1.s). The
// header sits at 0x180 so the classic partition table area at 0x1BE stays free
// for the ESP entry UEFI firmware needs. `H_VERSION` is the single product
// version (`tablestore::VERSION`), written here from this crate's inherited
// workspace version — stage1.s only reserves the bytes.
const H_VERSION: usize = 0x188; // u32: unified product version
const H_DATA_LBA: usize = 0x18C;
const H_S2_LBA: usize = 0x194;
const H_S2_SECS: usize = 0x198;
const H_K_MEM_MIB: usize = 0x19A; // kernel RAM footprint (image+bss+stack), MiB
const H_K_LBA: usize = 0x19C;
const H_K_SECS: usize = 0x1A0;
const H_K_LOAD: usize = 0x1A4;
const H_K_ENTRY: usize = 0x1A8;
const H_SYS_GUID: usize = 0x1AC; // 16-byte unique system id
const PART_TABLE: usize = 0x1BE; // classic MBR partition table (entry 1 = ESP)

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
    let kernel_mem_mib = kernel_mem_mib(&std::fs::read(KERNEL_ELF).expect("read kernel ELF"));

    // The UEFI loader PE binary, destined for \EFI\BOOT\BOOTX64.EFI.
    let uefi_loader = std::fs::read(UEFI_LOADER).expect("read uefi-loader.efi");

    // 3. Lay out the single image.
    assert_eq!(stage1.len(), SECTOR, "stage1 must be exactly one sector");
    let s2_secs = sectors(stage2.len());
    assert!(
        s2_secs <= STAGE2_MAX_SECTORS,
        "stage2 is {s2_secs} sectors, max {STAGE2_MAX_SECTORS}"
    );
    let k_secs = sectors(kernel.len());
    let esp_lba = align_up(KERNEL_LBA as u64 + k_secs as u64, 2048); // 1 MiB
    let data_lba = align_up(esp_lba + fat::ESP_SECTORS, 2048);
    assert!(
        data_lba + 4096 < IMG_SECTORS,
        "kernel + ESP too large; volume would be empty"
    );

    let mut img = vec![0u8; IMG_SECTORS as usize * SECTOR];
    img[..SECTOR].copy_from_slice(&stage1);
    put_u32(&mut img, H_VERSION, tablestore::VERSION);
    put_u64(&mut img, H_DATA_LBA, data_lba);
    put_u32(&mut img, H_S2_LBA, STAGE2_LBA);
    put_u16(&mut img, H_S2_SECS, s2_secs as u16);
    put_u16(&mut img, H_K_MEM_MIB, kernel_mem_mib);
    put_u32(&mut img, H_K_LBA, KERNEL_LBA);
    put_u32(&mut img, H_K_SECS, k_secs);
    put_u32(&mut img, H_K_LOAD, KERNEL_LOAD);
    put_u32(&mut img, H_K_ENTRY, KERNEL_LOAD);

    // Unique system GUID for this image. The kernel re-reads it from the MBR
    // and refuses to run on any disk whose GUID differs from the one it
    // booted with — so it can never touch a disk that isn't this one.
    let guid = random_guid();
    img[H_SYS_GUID..H_SYS_GUID + 16].copy_from_slice(&guid);

    // Partition-table entry 1: the EFI System Partition (type 0xEF). This is
    // what lets UEFI firmware discover the FAT16 volume and run BOOTX64.EFI;
    // the BIOS path and the kernel never read it. Entries 2-4 stay zero — the
    // TablesOS volume remains raw, unpartitioned space described only by the
    // TBLSBOOT header.
    let pe = &mut img[PART_TABLE..PART_TABLE + 16];
    pe[0] = 0x80; // bootable flag (ignored by UEFI, calms picky BIOSes)
    pe[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS start: "use LBA"
    pe[4] = 0xEF; // EFI System Partition
    pe[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS end
    pe[8..12].copy_from_slice(&(esp_lba as u32).to_le_bytes());
    pe[12..16].copy_from_slice(&(fat::ESP_SECTORS as u32).to_le_bytes());

    let s2_off = STAGE2_LBA as usize * SECTOR;
    img[s2_off..s2_off + stage2.len()].copy_from_slice(&stage2);
    let k_off = KERNEL_LBA as usize * SECTOR;
    img[k_off..k_off + kernel.len()].copy_from_slice(&kernel);

    let esp = fat::build_esp(&uefi_loader, esp_lba as u32);
    let e_off = esp_lba as usize * SECTOR;
    img[e_off..e_off + esp.len()].copy_from_slice(&esp);

    // Lay down an ALREADY-FORMATTED TablesOS volume (the OS never formats at
    // runtime — see SPECIFICATION.md). We format an in-memory volume of the
    // exact size the kernel will see (whole device − data_lba) and copy it in.
    let vol_sectors = IMG_SECTORS - data_lba;
    let mut st = tablestore::Store::format(tablestore::MemBlockDevice::new(vol_sectors))
        .expect("format in-memory volume");
    // Optionally fill the volume with the test dataset (`--seed`), so the
    // booted system comes up populated. Done before the snapshot below, so the
    // seeded rows are baked into the image exactly like any formatted volume.
    if std::env::args().any(|a| a == "--seed") {
        let stats = seed::seed(&mut st).expect("seed test data");
        println!(
            "seeded volume: {} addresses, {} people, {} distances, {} notes",
            stats.addresses, stats.people, stats.distances, stats.notes
        );
    }
    let vol = st.device_mut().snapshot();
    let voff = data_lba as usize * SECTOR;
    assert_eq!(vol.len(), img.len() - voff, "volume size mismatch");
    img[voff..].copy_from_slice(&vol);

    let image = out.join("tablesos.img");
    std::fs::write(&image, &img).expect("write image");
    println!(
        "hybrid BIOS+UEFI image {}: {} ({} MiB)\n  stage2 = {s2_secs} sectors, kernel = {k_secs} sectors ({kernel_mem_mib} MiB in RAM), ESP @ LBA {esp_lba}, volume @ LBA {data_lba}",
        tablestore::VERSION_STR,
        image.display(),
        img.len() / 1024 / 1024
    );
    println!(
        "Flash to a pendrive (whole device):  dd if={} of=/dev/sdX bs=4M conv=fsync",
        image.display()
    );

    // `--archive`: keep a copy of this build under `images/` named by version,
    // so a previous release stays around to test the "top up version" upgrade
    // path against (see Cargo.toml / kernel/src/upgrade.rs). The policy is:
    // whenever the version is bumped, archive the OUTGOING release first, then
    // bump — `images/` therefore accumulates one image per shipped version.
    if std::env::args().any(|a| a == "--archive") {
        let seeded = std::env::args().any(|a| a == "--seed");
        let images = manifest.join("images");
        std::fs::create_dir_all(&images).expect("create images/ dir");
        let name = format!(
            "tablesos-{}{}.img",
            tablestore::VERSION_STR,
            if seeded { "-seed" } else { "" }
        );
        let dst = images.join(&name);
        std::fs::copy(&image, &dst).expect("archive image");
        println!("archived {} → {}", tablestore::VERSION_STR, dst.display());
    }

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
    // `--usb-image <path>`: stage a specific image onto the emulated USB stick
    // before launching (always overwriting). This is what makes the "top up
    // version" upgrade testable from `cargo run`: point it at an older release
    // image (e.g. `images/tablesos-v0.1.0-seed.img`) and the booted OS will see
    // that volume on USB, ready to upgrade via Table List `[u]`. Re-run before
    // each attempt — a successful upgrade rewrites the stick to this version.
    if let Some(src) = arg_value("--usb-image") {
        std::fs::copy(&src, &usb_img)
            .unwrap_or_else(|e| panic!("stage --usb-image {src}: {e}"));
        println!("staged USB stick from {src}");
    } else {
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
    }

    let qemu = find_qemu();
    let mut cmd = Command::new(&qemu);

    // `--uefi`: boot through OVMF/EDK2 firmware (ships with QEMU) instead of
    // the legacy BIOS, exercising the BOOTX64.EFI path end to end. The vars
    // flash is a per-checkout writable copy so firmware boot-order writes
    // don't touch the QEMU installation.
    if std::env::args().any(|a| a == "--uefi") {
        let share = qemu
            .parent()
            .map(|p| p.join("share"))
            .unwrap_or_else(|| PathBuf::from("share"));
        let code = share.join("edk2-x86_64-code.fd");
        let vars_src = share.join("edk2-i386-vars.fd");
        let vars = out.join("uefi-vars.fd");
        if !code.exists() {
            eprintln!("UEFI firmware not found at {}", code.display());
            eprintln!("Install a QEMU build that ships EDK2, or boot the image elsewhere.");
            std::process::exit(1);
        }
        if !vars.exists() {
            std::fs::copy(&vars_src, &vars).expect("copy EDK2 vars template");
        }
        cmd.args([
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        ])
        .args(["-drive", &format!("if=pflash,format=raw,file={}", vars.display())]);
    }

    // Prefer a hardware accelerator, falling back to TCG software emulation if
    // it isn't available (`accel=A:tcg` picks the first usable one, so this
    // never fails to launch). Under pure TCG every guest instruction is
    // interpreted, which makes the kernel's full-screen framebuffer copies
    // crawl (~80 ms a present at high resolution); with the accelerator they run
    // near-native. WHPX needs the in-kernel irqchip disabled.
    let accel = if cfg!(target_os = "windows") {
        "whpx:tcg,kernel-irqchip=off"
    } else if cfg!(target_os = "macos") {
        "hvf:tcg"
    } else {
        "kvm:tcg"
    };
    let status = cmd
        .args(["-machine", &format!("pc,accel={accel}")])
        // TablesOS owns the whole machine and wants a large heap (five cached
        // full-screen background composites + the engine working set), which
        // the bootloader now carves out of free RAM (BIOS: largest E820 region;
        // UEFI: an AllocatePages block) — so give the VM generous RAM.
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
        // A USB-HID boot mouse on the same xHCI bus, so the HID input path is
        // exercised in QEMU. It reports relative motion in boot protocol, which
        // the kernel polls via the xHCI interrupt-IN endpoint. Unlike usb-kbd,
        // attaching a usb-mouse does not steal PS/2 keyboard input.
        .args(["-device", "usb-mouse,bus=xhci.0"])
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

/// Value of a `--flag value` command-line argument, if present.
fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    None
}

/// The kernel's total RAM footprint in MiB (rounded up): the highest
/// `p_vaddr + p_memsz` over the ELF's PT_LOAD segments, minus the 0x1000000
/// load address. Unlike the flat binary this includes NOLOAD `.bss` — the
/// small fallback heap and the boot stack — which the UEFI loader must reserve
/// at the load address before handing over (the BIOS path just assumes the RAM
/// is there). The kernel's main heap is reserved separately by the bootloader.
fn kernel_mem_mib(elf: &[u8]) -> u16 {
    assert!(
        elf.len() >= 64 && elf[..4] == [0x7F, b'E', b'L', b'F'] && elf[4] == 2 && elf[5] == 1,
        "kernel is not a 64-bit little-endian ELF"
    );
    let phoff = u64::from_le_bytes(elf[0x20..0x28].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(elf[0x36..0x38].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(elf[0x38..0x3A].try_into().unwrap()) as usize;
    let mut end = 0u64;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let p_type = u32::from_le_bytes(elf[p..p + 4].try_into().unwrap());
        if p_type != 1 {
            continue; // not PT_LOAD
        }
        let vaddr = u64::from_le_bytes(elf[p + 0x10..p + 0x18].try_into().unwrap());
        let memsz = u64::from_le_bytes(elf[p + 0x28..p + 0x30].try_into().unwrap());
        end = end.max(vaddr + memsz);
    }
    assert!(end > KERNEL_LOAD as u64, "no PT_LOAD segments found");
    let bytes = end - KERNEL_LOAD as u64;
    u16::try_from((bytes + (1 << 20) - 1) >> 20).expect("kernel footprint exceeds 64 GiB")
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
