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
.equ CHUNK_SECS,  64         # sectors per INT 13h read. Large transfers are
                             # reliable on real USB BIOSes; rapid back-to-back
                             # small reads are the thing that hangs them.
.equ H_DATA_LBA,  MBR + 0x1BC
.equ H_KERN_LBA,  MBR + 0x1CC
.equ H_KERN_SECS, MBR + 0x1D0

_start:
    # DIAG-D execution trace: raw INT 10h teletype markers (need no DS/stack) to
    # see how far stage 2 gets on real hardware. '1' = entered, '2' = segments +
    # stack + sti done, '3' = returned from the first print. Temporary.
    mov     ax, 0x0E31                 # '1'
    mov     bx, 7
    int     0x10

    cli
    xor     ax, ax
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     sp, 0x7000
    cld                            # forward string ops (BIOS entry DF unknown)
    mov     byte ptr [S2 + (drive - _start)], dl
    sti

    mov     ax, 0x0E32                 # '2'
    mov     bx, 7
    int     0x10

    mov     si, S2 + (msg_s2 - _start)
    call    print

    mov     ax, 0x0E33                 # '3'
    mov     bx, 7
    int     0x10

    # Do all real-mode BIOS disk work in TEXT mode with progress markers, then
    # switch to the VESA graphics framebuffer LAST. Once vesa_set runs, INT 10h
    # text output lands in a graphics LFB (invisible / stray glyphs on real HW),
    # so any diagnostic print must happen before it to be readable.
    call    a20_enable
    mov     si, S2 + (msg_a20 - _start)
    call    print
    call    unreal
    call    load_kernel
    mov     si, S2 + (msg_krn - _start)
    call    print

    call    vesa_set                    # set a video mode (with set-failure fallback)
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

# emit AL as one character (INT10 teletype + 0xE9)
emit:
    push    ax
    push    bx
    mov     ah, 0x0E
    mov     bx, 7
    int     0x10
    push    dx
    mov     dx, 0xE9
    out     dx, al
    pop     dx
    pop     bx
    pop     ax
    ret

# print AX as four hex digits
hex16:
    push    ax
    mov     al, ah
    call    hex8
    pop     ax
    call    hex8
    ret

# print AL as two hex digits
hex8:
    push    ax
    push    cx
    push    ax
    mov     cl, 4
    shr     al, cl
    call    .hnyb
    pop     ax
    call    .hnyb
    pop     cx
    pop     ax
    ret
.hnyb:
    and     al, 0x0F
    add     al, '0'
    cmp     al, '9'
    jbe     .hemit
    add     al, 7
.hemit:
    call    emit
    ret

crlf:
    push    ax
    mov     al, 13
    call    emit
    mov     al, 10
    call    emit
    pop     ax
    ret

die:
    mov     si, S2 + (msg_err - _start)
    call    print
.dh:
    hlt
    jmp     .dh

# Enable the A20 line and VERIFY it. Try the BIOS (INT 15h, AX=2401h) first,
# then the fast (port 0x92) method with the reset bit (bit 0) masked off so we
# never trigger a chipset reset. If A20 cannot be enabled we bail to `die`
# rather than let the >1 MiB kernel copy silently wrap on real hardware.
a20_enable:
    call    a20_check
    jnz     .a20_ok                 # already enabled
    mov     ax, 0x2401              # INT 15h: enable A20 via BIOS
    int     0x15
    call    a20_check
    jnz     .a20_ok
    in      al, 0x92               # fast A20 via System Control Port A
    test    al, 2
    jnz     .a20_92                 # bit already set: don't rewrite the port
    or      al, 2                  # set A20 (bit 1)
    and     al, 0xFE               # NEVER set bit 0 (INIT_NOW / fast reset)
    out     0x92, al
.a20_92:
    call    a20_check
    jnz     .a20_ok
    jmp     die                     # A20 stuck off -> cannot continue safely
.a20_ok:
    ret

# Test whether A20 is on via the classic 1 MiB wrap check: 0000:0500 and
# FFFF:0510 alias the same byte only while A20 is gated. Returns AL=1/ZF=0 when
# enabled, AL=0/ZF=1 when disabled. Clobbers AX (BX/DS/ES/SI/DI preserved).
a20_check:
    push    ds
    push    es
    push    si
    push    di
    push    bx
    xor     ax, ax
    mov     ds, ax
    not     ax
    mov     es, ax                  # ES = 0xFFFF
    mov     si, 0x0500
    mov     di, 0x0510              # FFFF:0510 = phys 0x100500, aliases 0x0500
    mov     bl, ds:[si]             # save originals
    mov     bh, es:[di]
    mov     byte ptr ds:[si], 0x00
    mov     byte ptr es:[di], 0xFF
    mov     al, ds:[si]             # reads back 0xFF iff the write wrapped
    mov     ds:[si], bl             # restore originals
    mov     es:[di], bh
    cmp     al, 0xFF
    mov     al, 1
    jne     .ac_done                # no wrap -> A20 enabled
    xor     al, al                  # wrap -> A20 disabled
.ac_done:
    or      al, al                  # set ZF: ZF=1 when disabled (AL=0)
    pop     bx
    pop     di
    pop     si
    pop     es
    pop     ds
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
# Reset-and-retry on failure: real hardware (esp. USB) often needs a disk
# reset (AH=00h) before a read succeeds. AX/DX are preserved across the INT 13h
# calls (which clobber AH) so the DAP is rebuilt correctly on every attempt.
disk_read:
    push    si
    push    cx
    mov     cx, 5                  # retry budget
.dr_try:
    mov     si, S2 + (dap - _start)
    mov     word ptr [si+0], 0x0010
    mov     [si+2], ax
    mov     word ptr [si+4], 0x0000
    mov     [si+6], dx
    mov     [si+8], ebx
    mov     dword ptr [si+12], 0
    pushad                         # INT 13h trashes DS/ES and can clobber the
    push    ds                     # 32-bit regs load_kernel carries (EBX=LBA,
    push    es                     # EDI=dest). Save/restore all of them. The
    mov     ah, 0x42              # unreal 4 GiB limit is re-armed at .dr_ok.
    mov     dl, byte ptr [S2 + (drive - _start)]
    int     0x13
    pop     es
    pop     ds
    popad
    jnc     .dr_ok
    pushad
    push    ds
    push    es
    xor     ah, ah                 # reset disk controller
    mov     dl, byte ptr [S2 + (drive - _start)]
    int     0x13
    pop     es
    pop     ds
    popad
    loop    .dr_try
    jmp     die
.dr_ok:
    # A real BIOS may service INT 13h through protected mode/SMM and reload
    # DS/ES with 64 KiB-limit descriptors, silently dropping the unreal 4 GiB
    # limit the kernel copy relies on. Popping the selectors restores their
    # value but NOT the cached limit, so re-enter unreal mode here. (QEMU keeps
    # the limit, which is why this only bites on bare metal.) `unreal` clobbers
    # EAX and BX, so preserve the LBA/scratch the caller carries in EAX/EBX.
    push    eax
    push    ebx
    call    unreal
    pop     ebx
    pop     eax
    pop     cx
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

# ---------------- VESA mode enumeration (DIAG-E) ----------------
# Walks the VBE mode list and reports, on screen, why vesa_set's filter
# (LFB + graphics + exactly 32bpp + >=1024x720) finds nothing on this GPU.
vesa_diag:
    mov     ax, 0x2000
    mov     es, ax
    xor     di, di
    mov     dword ptr es:[di], 0x32454256   # "VBE2"
    mov     ax, 0x4F00
    int     0x10
    cmp     ax, 0x004F
    je      .vd_have
    mov     si, S2 + (dm_novbe - _start)
    call    print
    jmp     .vd_halt
.vd_have:
    mov     si, S2 + (dm_ver - _start)
    call    print
    mov     ax, es:[di+4]                   # VbeVersion (BCD)
    call    hex16
    call    crlf
    mov     ax, es:[di+0x0E]
    mov     [S2 + (vm_off - _start)], ax
    mov     ax, es:[di+0x10]
    mov     [S2 + (vm_seg - _start)], ax
    xor     ax, ax
    mov     [S2 + (vm_idx - _start)], ax
    mov     [S2 + (d_cnt - _start)], ax
    mov     [S2 + (d_lfb - _start)], ax
    mov     [S2 + (d_b32w - _start)], ax
    mov     [S2 + (d_b32h - _start)], ax
    mov     [S2 + (d_anyw - _start)], ax
    mov     [S2 + (d_anyh - _start)], ax
    mov     byte ptr [S2 + (d_anyb - _start)], 0
    mov     byte ptr [S2 + (d_match - _start)], 0
.vd_loop:
    mov     ax, [S2 + (vm_seg - _start)]
    mov     fs, ax
    mov     si, [S2 + (vm_idx - _start)]
    shl     si, 1
    add     si, [S2 + (vm_off - _start)]
    mov     cx, fs:[si]
    cmp     cx, 0xFFFF
    je      .vd_done
    inc     word ptr [S2 + (vm_idx - _start)]
    inc     word ptr [S2 + (d_cnt - _start)]
    mov     [S2 + (cur_mode - _start)], cx
    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     ax, 0x4F01
    int     0x10
    cmp     ax, 0x004F
    jne     .vd_loop
    mov     ax, es:[di+0x00]               # ModeAttributes
    test    ax, 0x80                       # linear framebuffer available
    jz      .vd_loop
    test    ax, 0x10                       # graphics (not text)
    jz      .vd_loop
    inc     word ptr [S2 + (d_lfb - _start)]
    mov     dx, es:[di+0x12]               # XResolution
    mov     bx, es:[di+0x14]               # YResolution
    mov     al, byte ptr es:[di+0x19]      # BitsPerPixel
    cmp     dx, [S2 + (d_anyw - _start)]   # track widest LFB mode (any bpp)
    jbe     .vd_chk32
    mov     [S2 + (d_anyw - _start)], dx
    mov     [S2 + (d_anyh - _start)], bx
    mov     [S2 + (d_anyb - _start)], al
.vd_chk32:
    cmp     al, 32
    jne     .vd_loop
    cmp     dx, [S2 + (d_b32w - _start)]   # track widest 32bpp LFB mode
    jbe     .vd_chkmatch
    mov     [S2 + (d_b32w - _start)], dx
    mov     [S2 + (d_b32h - _start)], bx
.vd_chkmatch:
    cmp     dx, 1024                       # full filter: >=1024x720
    jb      .vd_loop
    cmp     bx, 720
    jb      .vd_loop
    mov     byte ptr [S2 + (d_match - _start)], 1
    jmp     .vd_loop
.vd_done:
    mov     si, S2 + (dm_cnt - _start)
    call    print
    mov     ax, [S2 + (d_cnt - _start)]
    call    hex16
    mov     si, S2 + (dm_lfb - _start)
    call    print
    mov     ax, [S2 + (d_lfb - _start)]
    call    hex16
    mov     si, S2 + (dm_match - _start)
    call    print
    mov     al, byte ptr [S2 + (d_match - _start)]
    call    hex8
    call    crlf
    mov     si, S2 + (dm_b32 - _start)
    call    print
    mov     ax, [S2 + (d_b32w - _start)]
    call    hex16
    mov     al, 'x'
    call    emit
    mov     ax, [S2 + (d_b32h - _start)]
    call    hex16
    call    crlf
    mov     si, S2 + (dm_any - _start)
    call    print
    mov     ax, [S2 + (d_anyw - _start)]
    call    hex16
    mov     al, 'x'
    call    emit
    mov     ax, [S2 + (d_anyh - _start)]
    call    hex16
    mov     si, S2 + (dm_bpp - _start)
    call    print
    mov     al, byte ptr [S2 + (d_anyb - _start)]
    call    hex8
    call    crlf
.vd_halt:
    hlt
    jmp     .vd_halt

# ---------------- VESA mode selection ----------------
# Pick the largest matching mode (LFB + graphics + 32bpp + >=1024x720) and SET it.
# A mode being *listed* does not mean 4F02 can *set* it (e.g. a 4:3 1920x1440 the
# panel rejects). On a set failure we lower an area ceiling and retry the next-
# largest matching mode, until one actually sets or none remain. Each attempt
# prints "V wxh"; a failed set adds "F", so a remaining failure is visible.
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
    mov     word ptr [S2 + (vs_ceil - _start)], 0xFFFF      # accept any area first
    mov     word ptr [S2 + (vs_ceil - _start) + 2], 0xFFFF

.vs_pass:
    mov     word ptr [S2 + (best_mode - _start)], 0xFFFF
    mov     word ptr [S2 + (best_area - _start)], 0
    mov     word ptr [S2 + (best_area - _start) + 2], 0
    mov     word ptr [S2 + (vm_idx - _start)], 0
.vs_scan:
    mov     ax, [S2 + (vm_seg - _start)]
    mov     fs, ax
    mov     si, [S2 + (vm_idx - _start)]
    shl     si, 1
    add     si, [S2 + (vm_off - _start)]
    mov     cx, fs:[si]
    cmp     cx, 0xFFFF
    je      .vs_scan_done
    inc     word ptr [S2 + (vm_idx - _start)]
    mov     [S2 + (cur_mode - _start)], cx
    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     ax, 0x4F01
    int     0x10
    cmp     ax, 0x004F
    jne     .vs_scan
    mov     ax, es:[di+0x00]            # attributes
    test    ax, 0x80                    # linear framebuffer
    jz      .vs_scan
    test    ax, 0x10                    # graphics mode
    jz      .vs_scan
    cmp     byte ptr es:[di+0x19], 32   # bpp
    jne     .vs_scan
    mov     ax, es:[di+0x12]            # width
    cmp     ax, 1024
    jb      .vs_scan
    mov     bx, es:[di+0x14]            # height
    cmp     bx, 720
    jb      .vs_scan
    mul     bx                          # DX:AX = width * height
    cmp     dx, word ptr [S2 + (vs_ceil - _start) + 2]   # skip area >= ceiling
    ja      .vs_scan
    jb      .vs_ceil_ok
    cmp     ax, word ptr [S2 + (vs_ceil - _start)]
    jae     .vs_scan
.vs_ceil_ok:
    cmp     dx, word ptr [S2 + (best_area - _start) + 2] # keep largest under ceiling
    jb      .vs_scan
    ja      .vs_take
    cmp     ax, word ptr [S2 + (best_area - _start)]
    jbe     .vs_scan
.vs_take:
    mov     word ptr [S2 + (best_area - _start)], ax
    mov     word ptr [S2 + (best_area - _start) + 2], dx
    mov     ax, [S2 + (cur_mode - _start)]
    mov     [S2 + (best_mode - _start)], ax
    jmp     .vs_scan

.vs_scan_done:
    cmp     word ptr [S2 + (best_mode - _start)], 0xFFFF
    je      die                         # no settable matching mode remains
    # read mode info; save fb params (used iff the set below succeeds)
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
    mov     al, byte ptr es:[di+0x20]   # RedFieldPosition
    test    al, al
    jz      .vs_fmt
    mov     byte ptr [S2 + (fb_fmt - _start)], 1   # red high -> BGR
.vs_fmt:
    mov     si, S2 + (msg_vtry - _start)
    call    print
    mov     ax, [S2 + (fb_w - _start)]
    call    hex16
    mov     al, 'x'
    call    emit
    mov     ax, [S2 + (fb_h - _start)]
    call    hex16
    mov     al, ' '
    call    emit
    mov     bx, [S2 + (best_mode - _start)]
    or      bx, 0x4000                  # request linear framebuffer
    mov     ax, 0x4F02
    int     0x10
    cmp     ax, 0x004F
    je      .vs_ok
    mov     al, 'F'                     # listed but unsettable; try a smaller one
    call    emit
    call    crlf
    mov     ax, word ptr [S2 + (best_area - _start)]
    mov     word ptr [S2 + (vs_ceil - _start)], ax
    mov     ax, word ptr [S2 + (best_area - _start) + 2]
    mov     word ptr [S2 + (vs_ceil - _start) + 2], ax
    jmp     .vs_pass
.vs_ok:
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
vs_ceil:    .long 0
fb_w:       .word 0
fb_h:       .word 0
fb_pitch:   .word 0
fb_addr:    .long 0
fb_bpp:     .byte 0
fb_fmt:     .byte 0
dap:        .space 16
d_cnt:      .word 0
d_lfb:      .word 0
d_b32w:     .word 0
d_b32h:     .word 0
d_anyw:     .word 0
d_anyh:     .word 0
d_anyb:     .byte 0
d_match:    .byte 0
msg_s2:     .asciz "TablesOS stage2\r\n"
msg_a20:    .asciz "a20 ok\r\n"
msg_krn:    .asciz "kernel loaded\r\n"
msg_lm:     .asciz "->long mode\r\n"
msg_err:    .asciz "stage2 error\r\n"
dm_novbe:   .asciz "no VBE\r\n"
dm_ver:     .asciz "VER="
dm_cnt:     .asciz "MODES="
dm_lfb:     .asciz " LFB="
dm_match:   .asciz " MATCH="
dm_b32:     .asciz "B32="
dm_any:     .asciz "ANY="
dm_bpp:     .asciz " b="
msg_vtry:   .asciz "V "

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
