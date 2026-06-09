# TablesOS stage 1 — DIAGNOSTIC build [DIAG-C].  TEMPORARY — see INVESTIGATION.md.
#
# This does NOT boot stage 2. It measures how many sectors a single INT 13h
# extended read (AH=42h) actually delivers on the target BIOS, and prints the
# result to screen, then halts. Restore the real loader (saved in INVESTIGATION.md)
# once the disk-read behaviour is understood.

.intel_syntax noprefix
.code16
.section .boot, "ax"
.globl _start

.equ S1,             0x7C00
.equ STAGE2_LBA,     1
.equ STAGE2_SECTORS, 63
.equ STAGE2_SEG,     0x0800        # 0x0800:0 = phys 0x8000
.equ DIAG_SECS,      16            # sectors requested in the one test read
.equ DIAG_BUF,       0x8000        # where the test read lands
.equ H_S2_SECS,      S1 + 0x1C8    # builder-patched stage2 sector count

_start:
    jmp     short start
    nop

start:
    cli
    xor     ax, ax
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     sp, S1
    cld
    sti
    mov     byte ptr [S1 + (drive - _start)], dl

    mov     si, S1 + (msg_s1 - _start)
    call    print

    # --- drive number ---
    mov     si, S1 + (m_dl - _start)
    call    print
    mov     al, byte ptr [S1 + (drive - _start)]
    call    hex8
    call    crlf

    # --- INT 13h extensions present? (AH=41h, BX=55AAh) ---
    mov     ah, 0x41
    mov     bx, 0x55AA
    mov     dl, byte ptr [S1 + (drive - _start)]
    int     0x13
    mov     bl, 0
    jnc     1f
    mov     bl, 1
1:
    mov     si, S1 + (m_ext - _start)
    call    print
    mov     al, bl
    call    hex8
    call    crlf

    # --- header stage2 sector count (proves the builder patched 0x1C8) ---
    mov     si, S1 + (m_s2n - _start)
    call    print
    mov     ax, word ptr [H_S2_SECS]
    call    hex16
    call    crlf

    # --- pre-fill the buffer with a sentinel so we can see what the read wrote ---
    mov     di, DIAG_BUF
    mov     al, 0xCC
    mov     cx, DIAG_SECS * 512
    rep     stosb

    # --- ONE extended read of DIAG_SECS sectors at LBA 1 ---
    mov     si, S1 + (dap - _start)
    mov     word ptr [si+0], 0x0010
    mov     word ptr [si+2], DIAG_SECS
    mov     word ptr [si+4], 0x0000
    mov     word ptr [si+6], STAGE2_SEG
    mov     dword ptr [si+8], STAGE2_LBA
    mov     dword ptr [si+12], 0
    mov     ah, 0x42
    mov     dl, byte ptr [S1 + (drive - _start)]
    int     0x13
    # capture carry, AH status, and the BIOS-updated transfer count immediately
    mov     bl, 0
    jnc     2f
    mov     bl, 1
2:
    mov     byte ptr [S1 + (d_cf - _start)], bl
    mov     byte ptr [S1 + (d_ah - _start)], ah
    mov     si, S1 + (dap - _start)
    mov     ax, word ptr [si+2]
    mov     word ptr [S1 + (d_got - _start)], ax

    # --- print "RD c=.. a=.. n=...." ---
    mov     si, S1 + (m_rd - _start)
    call    print
    mov     al, byte ptr [S1 + (d_cf - _start)]
    call    hex8
    mov     si, S1 + (m_a - _start)
    call    print
    mov     al, byte ptr [S1 + (d_ah - _start)]
    call    hex8
    mov     si, S1 + (m_n - _start)
    call    print
    mov     ax, word ptr [S1 + (d_got - _start)]
    call    hex16
    call    crlf

    # --- first byte of each of DIAG_SECS sectors: CC = not delivered ---
    mov     si, DIAG_BUF
    mov     cx, DIAG_SECS
3:
    mov     al, [si]
    call    hex8
    mov     al, ' '
    call    emit
    add     si, 512
    dec     cx
    jnz     3b
    call    crlf

    mov     si, S1 + (m_end - _start)
    call    print

    # DIAG-D: the read is proven good above; now hand off to stage 2 so its own
    # raw markers can show where it dies on real hardware.
    mov     dl, byte ptr [S1 + (drive - _start)]
    ljmp    0x0000, 0x8000

# ---------------- helpers ----------------
# print zero-terminated string at DS:SI (INT10 teletype + QEMU 0xE9)
print:
    push    ax
    push    bx
.pn:
    lodsb
    test    al, al
    jz      .pd
    call    emit
    jmp     .pn
.pd:
    pop     bx
    pop     ax
    ret

# emit AL as one character
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
    call    .nyb
    pop     ax
    call    .nyb
    pop     cx
    pop     ax
    ret
.nyb:
    and     al, 0x0F
    add     al, '0'
    cmp     al, '9'
    jbe     .e
    add     al, 7
.e:
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

# ---------------- data ----------------
drive:      .byte 0
d_cf:       .byte 0
d_ah:       .byte 0
d_got:      .word 0
dap:        .space 16
msg_s1:     .asciz "TablesOS stage1 [DIAG-F]\r\n"
m_dl:       .asciz "DL="
m_ext:      .asciz "EXT cf="
m_s2n:      .asciz "S2N="
m_rd:       .asciz "RD c="
m_a:        .asciz " a="
m_n:        .asciz " n="
m_end:      .asciz "DIAG END\r\n"

# --- custom header (kept identical so the image builder still patches it) ---
.org 0x1B0
.ascii  "TBLSBOOT"          # 0x1B0 format magic
.word   1                   # 0x1B8 os bios version
.word   0                   # 0x1BA reserved
.quad   0                   # 0x1BC data location LBA   (builder patches)
.long   1                   # 0x1C4 stage2 lba
.word   0                   # 0x1C8 stage2 sectors      (builder patches)
.word   0                   # 0x1CA reserved
.long   0                   # 0x1CC kernel lba          (builder patches)
.long   0                   # 0x1D0 kernel sectors      (builder patches)
.long   0x00200000          # 0x1D4 kernel load
.long   0x00200000          # 0x1D8 kernel entry
.org 0x1DC
.space  16                  # 0x1DC unique system GUID  (builder patches)

.org 0x1FE
.byte 0x55, 0xAA
