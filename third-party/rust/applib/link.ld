/*
 * Het linker-script van een HopOS-app-image (applib).
 *
 * Elke app linkt op hetzelfde canonieke adres: SlotBase(1) = 0x50000000,
 * met de tekst op +0x10000, zoals de Go-images (-T 0x50010000). De
 * stage-2-map van de kern legt het image op de echte partitie van het slot;
 * de app merkt daar niets van.
 *
 * Wat de kern bij plaatsing leest en patcht (RamStart, RamSize, SlotHint,
 * HopAbiVersion) staat in .data.hopabi: geïnitialiseerde data, dus met
 * bytes in het bestand (de ABI-versie leest de kern uit het ELF) en nooit
 * in .bss, die _start veegt.
 *
 * Boven .bss begint de heap, op een pagina; hij loopt tot de stack, die
 * _start bovenin de RAM-declaratie zet.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x50010000;
    __hopapp_image_start = .;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16) {
        KEEP(*(.data.hopabi))
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
        __hopapp_bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __hopapp_bss_end = .;
    }

    . = ALIGN(4096);
    __hopapp_heap_start = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
    }
}
