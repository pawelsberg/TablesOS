# TablesOS stage 2 — loaded at 0x8000 by stage 1 (real mode, DL = drive).
# Sets a VESA LFB mode, loads the kernel to 0x1000000 via unreal-mode INT13,
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
.equ KERNEL_DST,  0x1000000
.equ CHUNK_SECS,  64         # sectors per INT 13h read. Large transfers are
                             # reliable on real USB BIOSes; rapid back-to-back
                             # small reads are the thing that hangs them.
.equ H_DATA_LBA,  MBR + 0x18C
.equ H_KERN_MIB,  MBR + 0x19A
.equ H_KERN_LBA,  MBR + 0x19C
.equ H_KERN_SECS, MBR + 0x1A0

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
    call    find_heap                   # E820 scan (real mode, before unreal)
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

# Find the largest usable (E820 type 1) RAM region below 4 GiB that lies above
# the kernel footprint, and record it in heap_base/heap_size for BootInfo. The
# kernel runs identity-mapped 0..4 GiB and keeps no large .bss heap any more, so
# it allocates from this block instead. Leaves both 0 (kernel uses its small
# fallback heap) if E820 is unavailable or nothing qualifies. Real mode: must
# run before `unreal`/graphics. Clobbers EAX/EBX/ECX/EDX/SI/DI/ES.
find_heap:
    # heap_floor = align-up-2MiB(KERNEL_DST + kernel_mem_MiB * 1 MiB)
    movzx   eax, word ptr ds:[H_KERN_MIB]
    shl     eax, 20
    add     eax, KERNEL_DST
    add     eax, 0x1FFFFF
    and     eax, 0xFFE00000
    mov     [S2 + (heap_floor - _start)], eax
    mov     dword ptr [S2 + (heap_base - _start)], 0
    mov     dword ptr [S2 + (heap_size - _start)], 0
    mov     dword ptr [S2 + (e820_cont - _start)], 0
.fh_loop:
    push    ds
    pop     es                          # ES = DS = 0 for the E820 buffer
    mov     di, S2 + (e820buf - _start)
    mov     eax, 0xE820
    mov     edx, 0x534D4150             # 'SMAP'
    mov     ecx, 24
    mov     ebx, [S2 + (e820_cont - _start)]
    int     0x15
    jc      .fh_done                    # CF: end of list (or E820 unsupported)
    cmp     eax, 0x534D4150
    jne     .fh_done
    mov     [S2 + (e820_cont - _start)], ebx
    mov     si, S2 + (e820buf - _start)
    cmp     dword ptr [si+16], 1        # type 1 = usable
    jne     .fh_next
    cmp     dword ptr [si+4], 0         # base high dword: skip if >= 4 GiB
    jne     .fh_next
    mov     eax, [si+0]                 # base (low 32)
    mov     ecx, [si+8]                 # length low
    mov     edx, [si+12]                # length high
    test    edx, edx
    jnz     .fh_cap                     # length crosses 4 GiB -> cap end
    add     ecx, eax                    # end = base + length
    jnc     .fh_haveend
.fh_cap:
    mov     ecx, 0xFFFFF000             # cap just under 4 GiB
.fh_haveend:
    cmp     eax, [S2 + (heap_floor - _start)]   # clamp start up to heap_floor
    jae     .fh_clamped
    mov     eax, [S2 + (heap_floor - _start)]
.fh_clamped:
    cmp     eax, ecx                    # start >= end -> nothing usable here
    jae     .fh_next
    mov     edx, ecx
    sub     edx, eax                    # len = end - start
    cmp     edx, [S2 + (heap_size - _start)]
    jbe     .fh_next                    # not larger than the best so far
    mov     [S2 + (heap_size - _start)], edx
    mov     [S2 + (heap_base - _start)], eax
.fh_next:
    mov     ebx, [S2 + (e820_cont - _start)]
    test    ebx, ebx                    # 0 = last entry returned
    jnz     .fh_loop
.fh_done:
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

# ---------------- VESA mode selection + chooser ----------------
# Enumerate every qualifying mode (LFB + graphics + 32bpp + >=1024x720) into
# `mode_tab`, then let the user choose one. The default is the SMALLEST (the
# mode a fixed LCD panel is most likely to actually display — laptop panels
# happily "set" oversized modes via 4F02 but then show no signal). A short
# countdown auto-boots the default; pressing a number tries that mode and shows
# a colour test pattern, and Enter keeps it while ESC / 10 s reverts to the menu.
# All UI is drawn in TEXT mode (graphics is only entered while testing/keeping a
# mode), so it stays readable on hardware where INT 10h teletype into an LFB is
# invisible.
.equ MODE_MAX,     35        # table capacity (selectable 1..9 then A..Z)
.equ NCOLS,        3         # menu columns
.equ OFFER_TICKS,  91        # ~5 s auto-boot countdown (18.2 ticks/s)
.equ REVERT_TICKS, 182       # ~10 s revert window after picking a mode

# Walk the VBE mode list; append qualifying modes (mode,w,h) to mode_tab and
# track def_idx = the smallest-area entry. `die` if none qualify.
build_mode_table:
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
    mov     word ptr [S2 + (mode_cnt - _start)], 0
    mov     word ptr [S2 + (def_idx - _start)], 0
    mov     word ptr [S2 + (best_area - _start)], 0xFFFF     # +inf
    mov     word ptr [S2 + (best_area - _start) + 2], 0xFFFF
    mov     word ptr [S2 + (vm_idx - _start)], 0
.bm_scan:
    mov     ax, [S2 + (vm_seg - _start)]
    mov     fs, ax
    mov     si, [S2 + (vm_idx - _start)]
    shl     si, 1
    add     si, [S2 + (vm_off - _start)]
    mov     cx, fs:[si]
    cmp     cx, 0xFFFF
    je      .bm_done
    inc     word ptr [S2 + (vm_idx - _start)]
    mov     [S2 + (cur_mode - _start)], cx
    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     ax, 0x4F01
    int     0x10
    cmp     ax, 0x004F
    jne     .bm_scan
    mov     ax, es:[di+0x00]            # attributes
    test    ax, 0x80                    # linear framebuffer
    jz      .bm_scan
    test    ax, 0x10                    # graphics mode
    jz      .bm_scan
    cmp     byte ptr es:[di+0x19], 32   # bpp
    jne     .bm_scan
    mov     ax, es:[di+0x12]            # width
    cmp     ax, 1024
    jb      .bm_scan
    mov     bx, es:[di+0x14]            # height
    cmp     bx, 720
    jb      .bm_scan
    # qualifying mode: AX=width, BX=height, cur_mode set
    mov     si, [S2 + (mode_cnt - _start)]
    cmp     si, MODE_MAX
    jae     .bm_scan                    # table full: ignore the rest
    mov     bp, si                      # bp = idx*6 (entry = mode,w,h words)
    add     bp, si
    add     bp, si
    shl     bp, 1
    add     bp, S2 + (mode_tab - _start)
    mov     cx, [S2 + (cur_mode - _start)]
    mov     [bp], cx
    mov     [bp+2], ax
    mov     [bp+4], bx
    push    ax
    mul     bx                          # DX:AX = area; track smallest -> def_idx
    cmp     dx, word ptr [S2 + (best_area - _start) + 2]
    ja      .bm_nodef
    jb      .bm_setdef
    cmp     ax, word ptr [S2 + (best_area - _start)]
    jae     .bm_nodef
.bm_setdef:
    mov     word ptr [S2 + (best_area - _start)], ax
    mov     word ptr [S2 + (best_area - _start) + 2], dx
    mov     ax, [S2 + (mode_cnt - _start)]
    mov     [S2 + (def_idx - _start)], ax
.bm_nodef:
    pop     ax
    inc     word ptr [S2 + (mode_cnt - _start)]
    jmp     .bm_scan
.bm_done:
    cmp     word ptr [S2 + (mode_cnt - _start)], 0
    je      die                         # no qualifying mode at all
    call    sort_modes
    ret

# Selection-sort the whole table ascending by area so the menu lists every mode
# smallest-first (the smallest, most panel-friendly, is the default). def_idx := 0.
sort_modes:
    mov     ax, [S2 + (mode_cnt - _start)]
    mov     [S2 + (s_n - _start)], ax            # entries to place (all of them)
    mov     word ptr [S2 + (s_i - _start)], 0
.so_i:
    mov     ax, [S2 + (s_i - _start)]
    cmp     ax, [S2 + (s_n - _start)]
    jae     .so_cap
    mov     [S2 + (s_min - _start)], ax          # min := i
    mov     ax, [S2 + (s_i - _start)]
    inc     ax
    mov     [S2 + (s_j - _start)], ax
.so_j:
    mov     ax, [S2 + (s_j - _start)]
    cmp     ax, [S2 + (mode_cnt - _start)]
    jae     .so_swap
    call    area_of_idx                          # DX:AX = area(s_j)
    mov     [S2 + (s_ja - _start)], ax
    mov     [S2 + (s_ja - _start) + 2], dx
    mov     ax, [S2 + (s_min - _start)]
    call    area_of_idx                          # DX:AX = area(s_min)
    cmp     word ptr [S2 + (s_ja - _start) + 2], dx
    jb      .so_jless
    ja      .so_jn
    cmp     word ptr [S2 + (s_ja - _start)], ax
    jae     .so_jn
.so_jless:
    mov     ax, [S2 + (s_j - _start)]
    mov     [S2 + (s_min - _start)], ax
.so_jn:
    inc     word ptr [S2 + (s_j - _start)]
    jmp     .so_j
.so_swap:
    mov     ax, [S2 + (s_i - _start)]
    mov     bx, [S2 + (s_min - _start)]
    cmp     ax, bx
    je      .so_inext
    call    swap_entries
.so_inext:
    inc     word ptr [S2 + (s_i - _start)]
    jmp     .so_i
.so_cap:
    mov     word ptr [S2 + (def_idx - _start)], 0
    ret

# AX = table index -> DX:AX = width*height. Preserves BX,CX.
area_of_idx:
    push    bx
    push    cx
    mov     bx, ax
    add     bx, ax
    add     bx, ax
    shl     bx, 1
    add     bx, S2 + (mode_tab - _start)
    mov     ax, [bx+2]
    mov     cx, [bx+4]
    mul     cx
    pop     cx
    pop     bx
    ret

# Swap the 6-byte entries at indices AX and BX. Clobbers AX,BX,CX,DX,SI,DI.
swap_entries:
    mov     si, ax
    add     si, ax
    add     si, ax
    shl     si, 1
    add     si, S2 + (mode_tab - _start)
    mov     di, bx
    add     di, bx
    add     di, bx
    shl     di, 1
    add     di, S2 + (mode_tab - _start)
    mov     cx, 3
.se_sw:
    mov     ax, [si]
    mov     dx, [di]
    mov     [si], dx
    mov     [di], ax
    add     si, 2
    add     di, 2
    loop    .se_sw
    ret

# Set the graphics mode at table index AX (LFB) and store its fb params.
# Returns CF=0 on success, CF=1 if 4F01 or 4F02 failed (leaves text mode).
set_mode_idx:
    push    bx
    push    bp
    mov     bp, ax                      # bp = idx*6
    add     bp, ax
    add     bp, ax
    shl     bp, 1
    add     bp, S2 + (mode_tab - _start)
    mov     cx, [bp]                    # mode number
    mov     [S2 + (cur_mode - _start)], cx
    mov     ax, 0x2000
    mov     es, ax
    mov     di, 0x200
    mov     ax, 0x4F01
    int     0x10
    cmp     ax, 0x004F
    jne     .smi_fail
    mov     bx, [S2 + (cur_mode - _start)]
    or      bx, 0x4000                  # request linear framebuffer
    mov     ax, 0x4F02
    int     0x10
    cmp     ax, 0x004F
    jne     .smi_fail
    mov     ax, 0x2000                  # 4F02 may have reloaded ES
    mov     es, ax
    mov     di, 0x200
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
    jz      .smi_rgb
    mov     byte ptr [S2 + (fb_fmt - _start)], 1   # red high -> BGR
.smi_rgb:
    pop     bp
    pop     bx
    clc
    ret
.smi_fail:
    pop     bp
    pop     bx
    stc
    ret

# Paint a green/blue gradient over the whole framebuffer so a working panel
# shows something unmistakable (and a dead/garbled one is obvious). Uses the
# unreal 4 GiB limit on ES; INT 10h/16h above drop it, so re-arm here and do not
# reload ES until after the fill.
fill_test_pattern:
    call    unreal
    xor     si, si                      # si = y  (mul clobbers DX, so keep y in SI)
.ftp_row:
    cmp     si, [S2 + (fb_h - _start)]
    jae     .ftp_done
    mov     ax, si                      # stash y low byte (green) for the row
    mov     byte ptr [S2 + (ftp_g - _start)], al
    movzx   eax, si
    movzx   ecx, word ptr [S2 + (fb_pitch - _start)]
    mul     ecx                         # EDX:EAX = y*pitch (EDX clobbered, ok now)
    add     eax, [S2 + (fb_addr - _start)]
    mov     edi, eax                    # row destination
    xor     bx, bx                      # bx = x
.ftp_px:
    cmp     bx, [S2 + (fb_w - _start)]
    jae     .ftp_nextrow
    movzx   eax, bl                     # blue = x low byte
    mov     ah, byte ptr [S2 + (ftp_g - _start)]   # green = y low byte
    mov     es:[edi], eax
    add     edi, 4
    inc     bx
    jmp     .ftp_px
.ftp_nextrow:
    inc     si
    jmp     .ftp_row
.ftp_done:
    ret

# Wait up to AX BIOS ticks for a key. Returns AX = BIOS key (AH=scan, AL=ascii)
# or 0xFFFF on timeout. Clobbers nothing else.
wait_key_timeout:
    push    bx
    push    cx
    push    dx
    mov     bx, ax                      # bx = timeout in ticks
    xor     ah, ah
    int     0x1A                        # CX:DX = tick count
    mov     cx, dx                      # cx = start tick (low word)
.wk_loop:
    mov     ah, 1
    int     0x16                        # ZF=0 -> a key is waiting
    jnz     .wk_key
    push    cx
    xor     ah, ah
    int     0x1A                        # DX = now (low word)
    pop     cx
    mov     ax, dx
    sub     ax, cx                      # elapsed ticks (mod 65536)
    cmp     ax, bx
    jb      .wk_loop
    mov     ax, 0xFFFF                  # timed out
    jmp     .wk_done
.wk_key:
    xor     ah, ah
    int     0x16                        # AX = key
.wk_done:
    pop     dx
    pop     cx
    pop     bx
    ret

# Print AX as unsigned decimal (no leading zeros).
print_dec:
    push    ax
    push    bx
    push    cx
    push    dx
    mov     bx, 10
    xor     cx, cx
.pd_div:
    xor     dx, dx
    div     bx
    push    dx
    inc     cx
    test    ax, ax
    jnz     .pd_div
.pd_emit:
    pop     ax
    add     al, '0'
    call    emit
    loop    .pd_emit
    pop     dx
    pop     cx
    pop     bx
    pop     ax
    ret

# Clear to text mode and draw the resolution menu as an NCOLS grid (every mode,
# smallest-first, column-major), labelled 1..9 then A..Z with '*' on the
# default, then the prompt (plus an error note if menu_err is set).
draw_menu_text:
    mov     ax, 0x0003                  # text 80x25 (also clears the screen)
    int     0x10
    mov     si, S2 + (mnu_hdr - _start)
    call    print
    # rows = ceil(mode_cnt / NCOLS)
    mov     ax, [S2 + (mode_cnt - _start)]
    add     ax, NCOLS - 1
    xor     dx, dx
    mov     cx, NCOLS
    div     cx
    mov     [S2 + (dm_rows - _start)], ax
    mov     word ptr [S2 + (dm_r - _start)], 0
.dm_row:
    mov     ax, [S2 + (dm_r - _start)]
    cmp     ax, [S2 + (dm_rows - _start)]
    jae     .dm_after
    mov     word ptr [S2 + (dm_c - _start)], 0
.dm_col:
    mov     ax, [S2 + (dm_c - _start)]
    cmp     ax, NCOLS
    jae     .dm_eol
    mov     ax, [S2 + (dm_c - _start)]  # idx = c*rows + r (column-major)
    mul     word ptr [S2 + (dm_rows - _start)]
    add     ax, [S2 + (dm_r - _start)]
    cmp     ax, [S2 + (mode_cnt - _start)]
    jae     .dm_nextcol                 # no entry in this grid cell
    mov     [S2 + (dm_idx - _start)], ax
    call    print_cell
.dm_nextcol:
    inc     word ptr [S2 + (dm_c - _start)]
    jmp     .dm_col
.dm_eol:
    call    crlf
    inc     word ptr [S2 + (dm_r - _start)]
    jmp     .dm_row
.dm_after:
    cmp     byte ptr [S2 + (menu_err - _start)], 0
    je      .dm_prompt
    mov     si, S2 + (mnu_err - _start)
    call    print
    mov     byte ptr [S2 + (menu_err - _start)], 0
.dm_prompt:
    mov     si, S2 + (mnu_prompt - _start)
    call    print
    ret

# Print one fixed-width grid cell for the mode at index dm_idx:
# "<*|space><label>) WWWWxHHH[H]" padded to 16 columns. Width is always 4
# digits (>=1024), so padding is 4 spaces for 3-digit heights, 3 for 4-digit.
print_cell:
    mov     ax, [S2 + (dm_idx - _start)]
    cmp     ax, [S2 + (def_idx - _start)]
    jne     .pc_nomark
    mov     al, '*'
    jmp     .pc_mark
.pc_nomark:
    mov     al, ' '
.pc_mark:
    call    emit
    mov     ax, [S2 + (dm_idx - _start)]    # label: 1..9 then A..Z
    cmp     ax, 9
    jb      .pc_digit
    sub     ax, 9
    add     ax, 'A'
    jmp     .pc_lbl
.pc_digit:
    add     ax, '1'
.pc_lbl:
    call    emit
    mov     al, ')'
    call    emit
    mov     al, ' '
    call    emit
    mov     bx, [S2 + (dm_idx - _start)]
    add     bx, bx
    add     bx, [S2 + (dm_idx - _start)]
    shl     bx, 1                          # bx = idx*6
    add     bx, S2 + (mode_tab - _start)
    mov     ax, [bx+2]                      # width
    call    print_dec
    mov     al, 'x'
    call    emit
    mov     ax, [bx+4]                      # height
    call    print_dec
    mov     ax, [bx+4]
    mov     cx, 3                           # 4-digit height -> 3 trailing spaces
    cmp     ax, 1000
    jae     .pc_pad
    mov     cx, 4                           # 3-digit height -> 4 trailing spaces
.pc_pad:
    mov     al, ' '
.pc_ps:
    call    emit
    dec     cx
    jnz     .pc_ps
    ret

# Top-level: build the table, then run the menu loop. Returns with a graphics
# mode set and fb params stored (either the confirmed pick or the default).
vesa_set:
    call    build_mode_table
.vc_menu:
    call    draw_menu_text
    mov     ax, OFFER_TICKS
    call    wait_key_timeout
    cmp     ax, 0xFFFF
    je      .vc_default                 # countdown elapsed
    cmp     al, 0x0D                    # Enter -> boot default
    je      .vc_default
    cmp     al, 0x1B                    # ESC -> boot default
    je      .vc_default
    # decode 1..9 then A..Z (case-insensitive) to a 0-based index
    cmp     al, '1'
    jb      .vc_alpha
    cmp     al, '9'
    ja      .vc_alpha
    sub     al, '1'
    movzx   ax, al
    jmp     .vc_have_idx
.vc_alpha:
    cmp     al, 'a'
    jb      .vc_upper
    cmp     al, 'z'
    ja      .vc_upper
    sub     al, 0x20                     # to upper-case
.vc_upper:
    cmp     al, 'A'
    jb      .vc_menu
    cmp     al, 'Z'
    ja      .vc_menu
    sub     al, 'A'
    movzx   ax, al
    add     ax, 9
.vc_have_idx:
    cmp     ax, [S2 + (mode_cnt - _start)]
    jae     .vc_menu                    # no such mode
    mov     [S2 + (sel_idx - _start)], ax
    call    set_mode_idx                # AX = index
    jc      .vc_setfail
    call    fill_test_pattern
    mov     ax, REVERT_TICKS
    call    wait_key_timeout
    cmp     al, 0x0D
    je      .vc_keep                    # Enter -> keep this mode
    jmp     .vc_menu                    # ESC / timeout -> revert (menu re-clears)
.vc_keep:
    ret
.vc_setfail:
    mov     byte ptr [S2 + (menu_err - _start)], 1
    jmp     .vc_menu
.vc_default:
    mov     ax, [S2 + (def_idx - _start)]
    call    set_mode_idx
    jnc     .vc_ok
    # default refused to set: walk all modes, take the first that sets
    mov     word ptr [S2 + (tmp_i - _start)], 0
.vc_dl:
    mov     ax, [S2 + (tmp_i - _start)]
    cmp     ax, [S2 + (mode_cnt - _start)]
    jae     die
    call    set_mode_idx
    jnc     .vc_ok
    inc     word ptr [S2 + (tmp_i - _start)]
    jmp     .vc_dl
.vc_ok:
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
    # copy the 16-byte system GUID from the in-memory MBR (0x7C00+0x1AC)
    mov     esi, MBR + 0x1AC
    mov     edi, BOOTINFO + 0x30
    mov     ecx, 16
.cpguid:
    mov     al, ds:[esi]
    mov     ds:[edi], al
    inc     esi
    inc     edi
    dec     ecx
    jnz     .cpguid
    # rsdp_addr (0x40): BIOS path passes 0 — the kernel falls back to the
    # legacy EBDA/E0000 RSDP scan. Only the UEFI loader fills this in.
    mov     dword ptr ds:[BOOTINFO + 0x40], 0
    mov     dword ptr ds:[BOOTINFO + 0x44], 0
    # heap region (0x48 base, 0x50 size) from the E820 scan; both 0 -> kernel
    # uses its small fallback heap.
    mov     eax, [S2 + (heap_base - _start)]
    mov     ds:[BOOTINFO + 0x48], eax
    mov     dword ptr ds:[BOOTINFO + 0x4C], 0
    mov     eax, [S2 + (heap_size - _start)]
    mov     ds:[BOOTINFO + 0x50], eax
    mov     dword ptr ds:[BOOTINFO + 0x54], 0
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
best_area:  .long 0
mode_cnt:   .word 0
def_idx:    .word 0
sel_idx:    .word 0
tmp_i:      .word 0
dm_rows:    .word 0
dm_r:       .word 0
dm_c:       .word 0
dm_idx:     .word 0
s_n:        .word 0
s_i:        .word 0
s_j:        .word 0
s_min:      .word 0
s_ja:       .long 0
ftp_g:      .byte 0
menu_err:   .byte 0
heap_floor: .long 0
heap_base:  .long 0
heap_size:  .long 0
e820_cont:  .long 0
e820buf:    .space 24
mode_tab:   .space 210    # MODE_MAX(35) * {mode, width, height} words
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
mnu_hdr:    .asciz "TablesOS - choose display resolution\r\n\r\n"
mnu_prompt: .asciz "\r\nPress 1-9 or A-Z to try a mode (* = default).  Enter/ESC: boot default.\r\nAfter a mode is shown: Enter keeps it, ESC or 10s reverts.\r\n"
mnu_err:    .asciz "\r\n** that mode could not be set - pick another **\r\n"

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
