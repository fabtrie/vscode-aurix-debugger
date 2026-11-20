; RAM flash loader for the AURIX TC3xx PFLASH (see flash_loader.rs, which embeds the bytes).
;
; Runs on CPU0 from its PSPR with interrupts disabled. The debugger sets A4 to the control block
; and A5, A6 to the two slots (in CPU0's DSPR, or behind the code in its PSPR), PSW to supervisor
; mode and CPUx_FLASHCON1.MASKUECC (no uncorrectable ECC error, hence no DIE trap, for the erased
; PFLASH the checksum command may read), then releases the core. The loader turns off the CPU0 and
; safety watchdogs, reports ready, and runs the commands the debugger puts into two slots,
; alternately (slot 0, slot 1, slot 0, ...). It never returns: the debugger halts it.
;
; Watchdogs (measured on a TC377): a disable request (CON1.DR) shows in SR.DS only once ENDINIT
; is set again, and a cleared ENDINIT puts the watchdog into time-out mode, disabled or not. PFLASH
; erase and program commands of a CPU need Safety ENDINIT cleared (the DMU answers PROER
; otherwise; the debugger's own commands are exempt). So, as in the iLLD, Safety ENDINIT is cleared
; only for the command cycles that start an erase or a program and set again before the wait.
;
; Control block (A4):
;   +0x0000  status: READY (0x4C445231) once the watchdogs are off, WDT_FAIL (0xE0570000) if not
;   +0x0004  WDTCPU0SR after the attempt, +0x0008 WDTSSR after the attempt
;   +0x000C  CCNT / 256 when ready and after every command (the debugger calibrates with it)
;   +0x0010  CCNT / 256 spent waiting for erases, +0x0014 for programs, +0x0018 for blank checks,
;            +0x001C for the debugger (the next command); the debugger zeroes them and turns on
;            CCNT (CCTRL.CE) before the start
; Slot:
;   +0x0000  command: 1 program, 2 erase, 3 blank check, 4 checksums
;   +0x0004  address (non-cached, segment 0xA)
;   +0x0008  count: program: bytes (multiple of 32); erase: logical sectors in one command;
;            blank check: sectors, each checked with its own "verify erased" command;
;            checksums: runs
;   +0x000C  busy mask of the bank in DMU_HF_STATUS
;   +0x0010  blank check: logical sector size
;   +0x0014  result: DMU_HF_ERRSR & 0x7F of the last DMU command (of the failing one)
;   +0x001C  state: the debugger writes 1 (ready), last; the loader writes 2 (done) or 3 (failed,
;            and then stops)
;   +0x0020  data (the debugger's slot size): bytes to program; the blank check's bitmap (bit set: not erased); the
;            checksum runs as (address, bytes) pairs, replaced by their sums (A, B): per word w,
;            A += w, B += A; a page that reads all zero and is erased adds 0x9E3779B9 to B
;
; Registers: A10 is the watchdog CON0 the wdt_* routines work on (WDTSCON0 after the start),
; D12-D15 are theirs; A8, D8 and D9 are the timing's (A8 is a global address register: PSW.GW
; is set), A8 also the checksum's flash pointer. Assemble with Green Hills `astri -nooptimize`
; (32-bit encodings only: the simulator executes this code and decodes only those) and copy the
; .text words into LOADER in flash_loader.rs.

        .text
entry:
        disable
        movh.a  a10, 0xF003
        lea     a10, [a10]0x624C        ; WDTCPU0CON0
        jl      wdt_open
        jl      wdt_disable
        jl      wdt_close
        movh.a  a10, 0xF003
        lea     a10, [a10]0x62A8        ; WDTSCON0 (Safety ENDINIT) from here on
        jl      wdt_open
        jl      wdt_disable
        jl      wdt_close
        movh.a  a15, 0xF003
        movh    d3, 1                   ; DS may follow a little later: up to 0x10000 reads
wdt_check:
        ld.w    d1, [a15]0x6254         ; WDTCPU0SR
        ld.w    d2, [a15]0x62B0         ; WDTSSR
        and     d0, d1, d2
        jnz.t   d0, 3, wdt_ok           ; DS of both
        addi    d3, d3, -1
        jne     d3, 0, wdt_check
        st.w    [a4]4, d1
        st.w    [a4]8, d2
        movh    d0, 0xE057
        st.w    [a4]0, d0
stop:
        j       stop
wdt_ok:
        st.w    [a4]4, d1
        st.w    [a4]8, d2
        mfcr    d8, 0xFC04
        sh      d8, d8, -8
        st.w    [a4]0xC, d8
        movh    d0, 0x4C44
        addi    d0, d0, 0x5231
        st.w    [a4]0, d0
        movh.a  a12, 0xAF00             ; command area: KEY at +0x5554
        movh.a  a13, 0xF804             ; DMU_HF_STATUS at +0x10, DMU_HF_ERRSR at +0x34
        movh.a  a14, 0xAF01
        lea     a14, [a14]-0x55B0       ; 0xAF00AA50: SEQ_ADDR, SEQ_COUNT +8, SEQ_CMD +0x58
        movh.a  a15, 0xAF00
        lea     a15, [a15]0x55F0        ; LOAD_PAGE
slots:
        mov.aa  a7, a5
        jl      run_slot
        mov.aa  a7, a6
        jl      run_slot
        j       slots

; Waits for slot A7 to be ready and runs its command.
run_slot:
        mov.aa  a3, a11
        mfcr    d8, 0xFC04
poll:
        ld.w    d0, [a7]0x1C
        jne     d0, 1, poll
        lea     a8, [a4]0x1C            ; waited for the debugger
        jl      time_add
        ld.w    d1, [a7]0               ; command
        ld.w    d2, [a7]4               ; address
        ld.w    d3, [a7]8               ; count
        ld.w    d6, [a7]0xC             ; busy mask
        lea     a2, [a7]0x20            ; data
        mov     d0, 0xFA                ; clear status
        st.w    [a12]0x5554, d0
        mov     d0, 0
        jeq     d1, 1, program
        jeq     d1, 2, erase
        jeq     d1, 3, blank
        jeq     d1, 4, checksum
        mov     d0, 0x100               ; unknown command
        j       fail

erase:
        jl      wdt_open                ; Safety ENDINIT cleared for the command cycles
        st.w    [a14]0, d2
        st.w    [a14]8, d3
        mov     d0, 0x80
        st.w    [a14]0x58, d0
        mov     d0, 0x50
        st.w    [a14]0x58, d0
        dsync
        jl      wdt_close
        mfcr    d8, 0xFC04
erase_wait:
        ld.w    d0, [a13]0x10
        and     d0, d0, d6
        jne     d0, 0, erase_wait
        lea     a8, [a4]0x10
        jl      time_add
        ld.w    d0, [a13]0x34
        and     d0, d0, 0x7F
        jne     d0, 0, fail
        j       done

program:
        jeq     d3, 0, done
        mov     d0, 0x50                ; enter page mode (PFLASH)
        st.w    [a12]0x5554, d0
        dsync
page_wait:
        ld.w    d0, [a13]0x10
        and     d0, d0, d6
        jne     d0, 0, page_wait
        mov     d5, 32                  ; a page ...
        mov     d7, 0xAA
        and     d4, d2, 0xFF
        jne     d4, 0, load
        mov     d4, 256
        jlt.u   d3, d4, load
        mov     d5, 256                 ; ... or a burst: 256-byte aligned and 256 bytes left
        mov     d7, 0xA6
load:
        mov     d4, d5
load_loop:
        ld.d    e8, [a2+]8
        st.d    [a15]0, e8
        addi    d4, d4, -8
        jne     d4, 0, load_loop
        jl      wdt_open                ; Safety ENDINIT cleared for the command cycles
        st.w    [a14]0, d2
        mov     d0, 0
        st.w    [a14]8, d0
        mov     d0, 0xA0
        st.w    [a14]0x58, d0
        st.w    [a14]0x58, d7
        dsync
        jl      wdt_close
        mfcr    d8, 0xFC04
program_wait:
        ld.w    d0, [a13]0x10
        and     d0, d0, d6
        jne     d0, 0, program_wait
        lea     a8, [a4]0x14
        jl      time_add
        ld.w    d0, [a13]0x34
        and     d0, d0, 0x7F
        jne     d0, 0, fail
        add     d2, d2, d5
        sub     d3, d3, d5
        j       program

blank:
        ld.w    d5, [a7]0x10            ; sector size
        mov     d7, 0                   ; bitmap word
        mov     d10, 0                  ; bit in it
        mov     d11, 32
blank_loop:
        jeq     d3, 0, blank_end
        st.w    [a14]0, d2
        mov     d0, 1
        st.w    [a14]8, d0
        mov     d0, 0x80
        st.w    [a14]0x58, d0
        mov     d0, 0x5F
        st.w    [a14]0x58, d0
        dsync
        mfcr    d8, 0xFC04
blank_wait:
        ld.w    d0, [a13]0x10
        and     d0, d0, d6
        jne     d0, 0, blank_wait
        lea     a8, [a4]0x18
        jl      time_add
        ld.w    d0, [a13]0x34
        and     d0, d0, 0x7F
        and     d1, d0, 0x6F            ; anything but EVER fails
        jne     d1, 0, fail
        jz.t    d0, 4, blank_next       ; EVER clear: erased
        mov     d1, 1
        sh      d1, d1, d10
        or      d7, d7, d1
        mov     d1, 0xFA                ; clear EVER
        st.w    [a12]0x5554, d1
blank_next:
        addi    d10, d10, 1
        add     d2, d2, d5
        addi    d3, d3, -1
        jne     d10, d11, blank_loop
        st.w    [a2+]4, d7
        mov     d7, 0
        mov     d10, 0
        j       blank_loop
blank_end:
        st.w    [a2]0, d7
        mov     d0, 0
        j       done

; Sums of D3 runs (address, bytes) at A2, each a multiple of 32 bytes, written over them.
checksum:
        jeq     d3, 0, checksum_end
        ld.w    d2, [a2]0               ; run address
        ld.w    d4, [a2]4               ; run bytes
        mov.a   a8, d2
        mov     d10, 0                  ; A
        mov     d11, 0                  ; B
checksum_page:
        jeq     d4, 0, checksum_run_end
        mov     d7, 0                   ; OR of the page's words
        mov     d5, 8
checksum_word:
        ld.w    d0, [a8+]4
        or      d7, d7, d0
        add     d10, d10, d0
        add     d11, d11, d10
        addi    d5, d5, -1
        jne     d5, 0, checksum_word
        jne     d7, 0, checksum_next
        st.w    [a14]0, d2              ; all zero: programmed zeros, or erased?
        mov     d0, 0
        st.w    [a14]8, d0
        mov     d0, 0x80
        st.w    [a14]0x58, d0
        mov     d0, 0x56                ; verify erased page
        st.w    [a14]0x58, d0
        dsync
checksum_wait:
        ld.w    d0, [a13]0x10
        and     d0, d0, d6
        jne     d0, 0, checksum_wait
        ld.w    d0, [a13]0x34
        and     d0, d0, 0x7F
        and     d1, d0, 0x6F            ; anything but EVER fails
        jne     d1, 0, fail
        jnz.t   d0, 4, checksum_zeros   ; EVER: not erased, the zeros are data
        movh    d1, 0x9E37
        addi    d1, d1, 0x79B9
        add     d11, d11, d1            ; erased: the sums cannot match the image
        j       checksum_next
checksum_zeros:
        mov     d1, 0xFA                ; clear EVER
        st.w    [a12]0x5554, d1
checksum_next:
        addi    d2, d2, 32
        addi    d4, d4, -32
        j       checksum_page
checksum_run_end:
        st.w    [a2]0, d10
        st.w    [a2]4, d11
        lea     a2, [a2]8
        addi    d3, d3, -1
        j       checksum
checksum_end:
        mov     d0, 0

done:
        st.w    [a7]0x14, d0
        mfcr    d8, 0xFC04
        sh      d8, d8, -8
        st.w    [a4]0xC, d8
        mov     d0, 2
        st.w    [a7]0x1C, d0
        ji      a3

fail:
        st.w    [a7]0x14, d0
        mov     d0, 3
        st.w    [a7]0x1C, d0
dead:
        j       dead

; time_add: adds (CCNT - D8) / 256 to the word at A8. Uses D8, D9; returns through A11.
time_add:
        mfcr    d9, 0xFC04
        sub     d9, d9, d8
        sh      d9, d9, -8
        ld.w    d8, [a8]0
        add     d8, d8, d9
        st.w    [a8]0, d8
        ji      a11

; The ENDINIT routines of the iLLD for the watchdog whose CON0 is at A10 (CON1 at +4). The
; password is CON0.PW with its low 6 bits inverted; D14 = REL and password. They use D12-D15 and
; return through A11.
;
; wdt_open: clears ENDINIT (password access if locked, then modify access), waits for it.
wdt_open:
        ld.w    d12, [a10]0
        extr.u  d13, d12, 2, 14
        xor     d13, d13, 0x3F
        sh      d13, d13, 2
        movh    d14, 0xFFFF
        and     d14, d12, d14
        or      d14, d14, d13
        jz.t    d12, 1, wdt_modify      ; not locked: no password access needed
        or      d15, d14, 1             ; password access: ENDINIT 1, LCK 0
        st.w    [a10]0, d15
wdt_modify:
        or      d15, d14, 2             ; modify access: ENDINIT 0, LCK 1
        st.w    [a10]0, d15
        mov     d13, 0x1000
wdt_wait:
        ld.w    d12, [a10]0
        jz.t    d12, 0, wdt_opened
        addi    d13, d13, -1
        jne     d13, 0, wdt_wait
wdt_opened:
        ji      a11                     ; (if ENDINIT stayed set, the DS check or PROER says so)

; wdt_disable: sets CON1.DR (ENDINIT cleared); DS follows when ENDINIT is set again.
wdt_disable:
        ld.w    d12, [a10]4
        or      d12, d12, 8
        st.w    [a10]4, d12
        ji      a11

; wdt_close: sets ENDINIT again (password access, then modify access with ENDINIT 1, LCK 1).
wdt_close:
        ld.w    d12, [a10]0
        extr.u  d13, d12, 2, 14
        xor     d13, d13, 0x3F
        sh      d13, d13, 2
        movh    d14, 0xFFFF
        and     d14, d12, d14
        or      d14, d14, d13
        or      d15, d14, 1             ; password access
        st.w    [a10]0, d15
        or      d15, d14, 3             ; modify access: ENDINIT 1, LCK 1
        st.w    [a10]0, d15
        ld.w    d12, [a10]0
        ji      a11
