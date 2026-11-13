ENTRY(main)
SECTIONS {
  .text 0x80000000 : { *(.text*) *(.rodata*) *(.init_array*) *(.ctors*) *(.dtors*) }
  .data 0x70000000 : { *(.data*) *(.sdata*) *(.sdata2*) *(.zdata*) }
  .bss : { *(.bss*) *(.sbss*) *(.zbss*) *(COMMON) }
}
