/* TablesOS kernel: flat, identity-mapped, loaded & run at 16 MiB.
   `_start` is forced first so the entry point == load address (0x1000000),
   which is all the custom bootloaders need to know. 16 MiB rather than 2 MiB
   because UEFI firmware keeps things in low RAM that must survive boot —
   OVMF parks ACPI NVS at 8 MiB — and the kernel footprint (image + 256 MiB
   .bss heap) must not cover them. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

SECTIONS {
  . = 0x1000000;

  .text : {
    *(.text._start)
    *(.text .text.*)
  }
  .rodata : { *(.rodata .rodata.*) }
  .data   : { *(.data .data.*) *(.got .got.*) }

  .bss (NOLOAD) : {
    . = ALIGN(16);
    __bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(16);
    __boot_stack_bottom = .;
    . += 0x20000;            /* 128 KiB boot stack */
    __boot_stack_top = .;
    __bss_end = .;
  }

  /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment*) }
}
