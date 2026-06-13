# TablesOS stage 1 — the custom MBR.
# BIOS loads this 512-byte sector at 0x7C00 in 16-bit real mode, DL = drive.
#
# Sector-0 layout (see boot/layout.md): the TBLSBOOT header lives at 0x180,
# leaving 0x1BE..0x1FD for a real 4-entry MBR partition table. The builder
# writes one partition entry there (type 0xEF — EFI System Partition) so UEFI
# firmware can find the FAT16 ESP carrying \EFI\BOOT\BOOTX64.EFI. The BIOS
# path ignores the table entirely: this boot code runs and chains stage 2.
#
# Toolchain note: only a PE/COFF GNU `as` is available (no ELF assembler or
# linker). So this is relocation-free: the load base is baked in as the
# constant expression `0x7C00 + (sym - _start)` (a same-section difference,
# resolved at assembly time — no relocation), exactly like a NASM `org`. The
# object becomes a flat binary via `objcopy -O binary` with no link step.
#
# Disk-read note: one large INT 13h extended read (AH=42h) of all 63 stage-2
# sectors. Verified reliable on real USB BIOSes (Byd Tracer); rapid
# back-to-back small reads are what hangs them.

.intel_syntax noprefix
.code16
.section .boot, "ax"
.globl _start

.equ S1,             0x7C00
.equ STAGE2_LBA,     1
.equ STAGE2_SECTORS, 63
.equ STAGE2_SEG,     0x0800        # 0x0800:0 = phys 0x8000

_start:
    jmp     short start
    nop

start:
    cli
    xor     ax, ax
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     sp, S1                 # stack just below us
    cld
    sti
    mov     byte ptr [S1 + (drive - _start)], dl

    mov     si, S1 + (msg_s1 - _start)
    call    print

    # read stage 2 via INT 13h extended read (AH=42h)
    mov     si, S1 + (dap - _start)
    mov     word ptr [si+0], 0x0010
    mov     word ptr [si+2], STAGE2_SECTORS
    mov     word ptr [si+4], 0x0000
    mov     word ptr [si+6], STAGE2_SEG
    mov     dword ptr [si+8], STAGE2_LBA
    mov     dword ptr [si+12], 0
    mov     ah, 0x42
    mov     dl, byte ptr [S1 + (drive - _start)]
    int     0x13
    jc      disk_err

    mov     si, S1 + (msg_jmp - _start)
    call    print

    mov     dl, byte ptr [S1 + (drive - _start)]
    ljmp    0x0000, 0x8000

disk_err:
    mov     si, S1 + (msg_err - _start)
    call    print
halt:
    hlt
    jmp     halt

# print zero-terminated string at DS:SI (INT10 + QEMU 0xE9)
print:
    push    ax
    push    bx
.pn:
    lodsb
    test    al, al
    jz      .pd
    mov     ah, 0x0E
    mov     bx, 7
    int     0x10
    push    dx
    mov     dx, 0xE9
    out     dx, al
    pop     dx
    jmp     .pn
.pd:
    pop     bx
    pop     ax
    ret

drive:      .byte 0
dap:        .space 16
msg_s1:     .asciz "TablesOS stage1\r\n"
msg_jmp:    .asciz "->stage2\r\n"
msg_err:    .asciz "stage1 disk error\r\n"

# --- custom TBLSBOOT header (must end before the partition table at 0x1BE) ---
.org 0x180
.ascii  "TBLSBOOT"          # 0x180 format magic
.word   2                   # 0x188 os loader format version (2: header @0x180)
.word   0                   # 0x18A reserved
.quad   0                   # 0x18C data location LBA   (builder patches)
.long   1                   # 0x194 stage2 lba
.word   0                   # 0x198 stage2 sectors      (builder patches)
.word   0                   # 0x19A kernel mem footprint, MiB (builder patches;
                            #       flat image + .bss + stack — UEFI loader
                            #       reserves this much RAM at the load address)
.long   0                   # 0x19C kernel lba          (builder patches)
.long   0                   # 0x1A0 kernel sectors      (builder patches)
.long   0x01000000          # 0x1A4 kernel load
.long   0x01000000          # 0x1A8 kernel entry
.space  16                  # 0x1AC unique system GUID  (builder patches)
.word   0                   # 0x1BC reserved

# --- MBR partition table (0x1BE): builder writes entry 1 = EFI System
# Partition (type 0xEF) covering the FAT16 ESP. Entries 2-4 stay zero. ---
.org 0x1FE
.byte 0x55, 0xAA
