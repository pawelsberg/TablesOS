/* TablesOS kernel: flat, identity-mapped, loaded & run at 2 MiB.
   `_start` is forced first so the entry point == load address (0x200000),
   which is all the custom bootloader needs to know. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

SECTIONS {
  . = 0x200000;

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
