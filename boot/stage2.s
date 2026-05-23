# TablesOS stage 2 — loaded at 0x8000 by stage 1 (real mode, DL = drive).
# Sets a VESA LFB mode, loads the kernel to 0x200000 via unreal-mode INT13,
# builds BootInfo + identity page tables, enters long mode, jumps to kernel.
#
# Relocation-free (see stage1.s): in-image symbol X has run-time address
# `S2 + (X - _start)`; branches are relative; objcopy -O binary, no linker.

.intel_syntax noprefix
.code16
.section .boot, "ax"
.globl _start

.equ S2,          0x8000
.equ MBR,         0x7C00
.equ BOOTINFO,    0x7000
.equ BOUNCE,      0x10000          # seg 0x1000
.equ PML4,        0x70000
.equ KERNEL_DST,  0x200000
.equ CHUNK_SECS,  64
.equ H_DATA_LBA,  MBR + 0x1BC
.equ H_KERN_LBA,  MBR + 0x1CC
.equ H_KERN_SECS, MBR + 0x1D0

_start:
    cli
    xor     ax, ax
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     sp, 0x7000
    mov     byte ptr [S2 + (drive - _start)], dl
    sti

    mov     si, S2 + (msg_s2 - _start)
    call    print

    call    vesa_set
    call    a20_enable
    call    unreal
    call    load_kernel
    call    build_bootinfo
    call    build_paging

    mov     si, S2 + (msg_lm - _start)
    call    print

    cli
    lgdt    [S2 + (gdt64_desc - _start)]
    mov     eax, cr4
    or      eax, 1 << 5                 # CR4.PAE
    mov     cr4, eax
    mov     eax, PML4
    mov     cr3, eax
    mov     ecx, 0xC0000080             # EFER
    rdmsr
    or      eax, 1 << 8                 # LME
    wrmsr
    mov     eax, cr0
    or      eax, 0x80000001             # PG | PE
    mov     cr0, eax
    # 32-bit far jump into the 64-bit code segment (selector 0x08)
    .byte   0x66, 0xEA
    .long   S2 + (long_mode - _start)
    .word   0x08

# ---------------- real-mode helpers ----------------
print:
    push    ax
    push    bx
.pl:
    lodsb
    test    al, al
    jz      .pe
    mov     ah, 0x0E
    mov     bx, 7
    int     0x10
    push    dx
    mov     dx, 0xE9
    out     dx, al
    pop     dx
    jmp     .pl
.pe:
    pop     bx
    pop     ax
    ret

die:
    mov     si, S2 + (msg_err - _start)
    call    print
.dh:
    hlt
    jmp     .dh

a20_enable:
    in      al, 0x92
    or      al, 2
    out     0x92, al
    ret

# Enter unreal mode: DS/ES keep base 0 but gain a 4 GiB limit. The segment
# regs must NOT be reloaded in real mode afterwards or the big limit is lost.
unreal:
    cli
    lgdt    [S2 + (gdt32_desc - _start)]
    mov     eax, cr0
    or      al, 1
    mov     cr0, eax
    mov     bx, 0x10
    mov     ds, bx
    mov     es, bx
    mov     eax, cr0
    and     al, 0xFE
    mov     cr0, eax
    xor     ax, ax
    mov     ds, ax                      # base 0, cached 4 GiB limit retained
    mov     es, ax
    sti
    ret

# Read AX sectors at LBA EBX into real-mode buffer DX:0000.
disk_read:
    push    si
    mov     si, S2 + (dap - _start)
    mov     word ptr [si+0], 0x0010
    mov     [si+2], ax
    mov     word ptr [si+4], 0x0000
    mov     [si+6], dx
    mov     [si+8], ebx
    mov     dword ptr [si+12], 0
    mov     ah, 0x42
    mov     dl, byte ptr [S2 + (drive - _start)]
    int     0x13
    jc      die
    pop     si
    ret

# Load the kernel image to KERNEL_DST through a 64 KiB bounce buffer.
load_kernel:
    mov     ebx, ds:[H_KERN_LBA]
    mov     edx, ds:[H_KERN_SECS]
    mov     edi, KERNEL_DST
.lk_loop:
    test    edx, edx
    jz      .lk_done
    mov     ecx, CHUNK_SECS
    cmp     edx, ecx
    jae     .lk_have
    mov     ecx, edx
.lk_have:
    push    edx
    push    ecx
    mov     ax, cx
    mov     dx, 0x1000
    call    disk_read
    pop     ecx
    push    ecx
    shl     ecx, 7                      # sectors * 512 / 4 dwords
    mov     esi, BOUNCE
.lk_copy:
    mov     eax, ds:[esi]
    mov     es:[edi], eax
    add     esi, 4
    add     edi, 4
    dec     ecx
    jnz     .lk_copy
    pop     ecx
    pop     edx
    add     ebx, ecx
    sub     edx, ecx
    jmp     .lk_loop
.lk_done:
    ret

# ---------------- VESA mode selection ----------------
vesa_set:
    mov     ax, 0x2000
    mov     es, ax
    xor     di, di
    mov     dword ptr es:[di], 0x32454256   # "VBE2"
    mov     ax, 0x4F00
    int     0x10
    cmp     ax, 0x004F
    jne     die

    mov     ax, es:[di+0x0E]
    mov     [S2 + (vm_off - _start)], ax
    mov     ax, es:[di+0x10]
    mov     [S2 + (vm_seg - _start)], ax

.scan:
    mov     ax, [S2 + (vm_seg - _start)]
    mov     fs, ax
    mov     si, [S2 + (vm_idx - _start)]
    shl     si, 1
    add     si, [S2 + (vm_off - _start)]
    mov     cx, fs:[si]
    cmp     cx, 0xFFFF
    je      .scan_done
    inc     word ptr [S2 + (vm_idx - _start)]
    mov     [S2 + (cur_mode - _start)], cx

    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     ax, 0x4F01
    int     0x10
    cmp     ax, 0x004F
    jne     .scan

    mov     ax, es:[di+0x00]            # attributes
    test    ax, 0x80                    # linear framebuffer
    jz      .scan
    test    ax, 0x10                    # graphics mode
    jz      .scan
    cmp     byte ptr es:[di+0x19], 32   # bpp
    jne     .scan
    mov     ax, es:[di+0x12]            # width
    cmp     ax, 1024
    jb      .scan
    mov     bx, es:[di+0x14]            # height
    cmp     bx, 720
    jb      .scan
    mul     bx                          # DX:AX = width * height
    cmp     dx, word ptr [S2 + (best_area - _start) + 2]
    ja      .take
    jb      .scan
    cmp     ax, word ptr [S2 + (best_area - _start)]
    jbe     .scan
.take:
    mov     word ptr [S2 + (best_area - _start)], ax
    mov     word ptr [S2 + (best_area - _start) + 2], dx
    mov     ax, [S2 + (cur_mode - _start)]
    mov     [S2 + (best_mode - _start)], ax
    jmp     .scan

.scan_done:
    cmp     word ptr [S2 + (best_mode - _start)], 0xFFFF
    je      die

    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     cx, [S2 + (best_mode - _start)]
    mov     ax, 0x4F01
    int     0x10

    mov     ax, es:[di+0x12]
    mov     [S2 + (fb_w - _start)], ax
    mov     ax, es:[di+0x14]
    mov     [S2 + (fb_h - _start)], ax
    mov     ax, es:[di+0x10]
    mov     [S2 + (fb_pitch - _start)], ax
    mov     eax, es:[di+0x28]
    mov     [S2 + (fb_addr - _start)], eax
    mov     al, byte ptr es:[di+0x19]
    shr     al, 3
    mov     byte ptr [S2 + (fb_bpp - _start)], al
    mov     byte ptr [S2 + (fb_fmt - _start)], 0   # 0 = RGB
    mov     al, byte ptr es:[di+0x20]   # RedFieldPosition (VBE mode info)
    test    al, al
    jz      .fmt_done                   # red at bit 0 -> RGB
    mov     byte ptr [S2 + (fb_fmt - _start)], 1   # red high -> BGR
.fmt_done:
    mov     bx, [S2 + (best_mode - _start)]
    or      bx, 0x4000
    mov     ax, 0x4F02
    int     0x10
    cmp     ax, 0x004F
    jne     die
    ret

# ---------------- BootInfo + paging ----------------
build_bootinfo:
    mov     di, BOOTINFO
    mov     dword ptr ds:[di+0x00], 0x5342544F
    movzx   eax, word ptr [S2 + (fb_w - _start)]
    mov     ds:[di+0x04], eax
    movzx   eax, word ptr [S2 + (fb_h - _start)]
    mov     ds:[di+0x08], eax
    movzx   eax, word ptr [S2 + (fb_pitch - _start)]
    mov     ds:[di+0x0C], eax
    mov     eax, [S2 + (fb_addr - _start)]
    mov     ds:[di+0x10], eax
    mov     dword ptr ds:[di+0x14], 0
    mov     al, byte ptr [S2 + (fb_bpp - _start)]
    mov     ds:[di+0x18], al
    mov     al, byte ptr [S2 + (fb_fmt - _start)]
    mov     ds:[di+0x19], al
    mov     eax, ds:[H_DATA_LBA]
    mov     ds:[di+0x20], eax
    mov     eax, ds:[H_DATA_LBA+4]
    mov     ds:[di+0x24], eax
    mov     al, byte ptr [S2 + (drive - _start)]
    mov     ds:[di+0x28], al
    # copy the 16-byte system GUID from the in-memory MBR (0x7C00+0x1DC)
    mov     esi, MBR + 0x1DC
    mov     edi, BOOTINFO + 0x30
    mov     ecx, 16
.cpguid:
    mov     al, ds:[esi]
    mov     ds:[edi], al
    inc     esi
    inc     edi
    dec     ecx
    jnz     .cpguid
    ret

# Identity-map 0..4 GiB with 2 MiB pages.
build_paging:
    mov     edi, PML4
    mov     ecx, (6 * 4096) / 4
    xor     eax, eax
.bz:
    mov     ds:[edi], eax
    add     edi, 4
    dec     ecx
    jnz     .bz

    mov     edi, PML4
    mov     dword ptr ds:[edi], 0x71000 + 3
    mov     edi, 0x71000
    mov     dword ptr ds:[edi+0x00], 0x72000 + 3
    mov     dword ptr ds:[edi+0x08], 0x73000 + 3
    mov     dword ptr ds:[edi+0x10], 0x74000 + 3
    mov     dword ptr ds:[edi+0x18], 0x75000 + 3

    mov     edi, 0x72000
    xor     ecx, ecx
    mov     eax, 0x83
.pd:
    mov     ds:[edi], eax
    mov     dword ptr ds:[edi+4], 0
    add     eax, 0x200000
    add     edi, 8
    inc     ecx
    cmp     ecx, 2048
    jb      .pd
    ret

# ---------------- 64-bit entry ----------------
.code64
long_mode:
    mov     ax, 0x10
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     fs, ax
    mov     gs, ax
    mov     rsp, 0x90000
    mov     rdi, BOOTINFO
    mov     eax, KERNEL_DST
    jmp     rax

# ---------------- data ----------------
.code16
drive:      .byte 0
vm_off:     .word 0
vm_seg:     .word 0
vm_idx:     .word 0
cur_mode:   .word 0
best_mode:  .word 0xFFFF
best_area:  .long 0
fb_w:       .word 0
fb_h:       .word 0
fb_pitch:   .word 0
fb_addr:    .long 0
fb_bpp:     .byte 0
fb_fmt:     .byte 0
dap:        .space 16
msg_s2:     .asciz "TablesOS stage2\r\n"
msg_lm:     .asciz "->long mode\r\n"
msg_err:    .asciz "stage2 error\r\n"

.align 8
gdt32:
    .quad   0
    .quad   0x00CF9A000000FFFF
    .quad   0x00CF92000000FFFF
gdt32_desc:
    .word   gdt32_desc - gdt32 - 1
    .long   S2 + (gdt32 - _start)

.align 8
gdt64:
    .quad   0
    .quad   0x00AF9A000000FFFF
    .quad   0x00CF92000000FFFF
gdt64_desc:
    .word   gdt64_desc - gdt64 - 1
    .long   S2 + (gdt64 - _start)
