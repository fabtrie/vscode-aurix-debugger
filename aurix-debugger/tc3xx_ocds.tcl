# Generic OCDS multi-core setup for AURIX TC3xx devices (OpenOCD and aurix-dbg).
#
# Load it after the target cfg (which creates <chip>.cpu<n> with -coreid and -dbgbase), set the
# options below if needed and call tc3xx_ocds_setup:
#
#   script [file join [file dirname [info script]] tc3xx_ocds.tcl]
#   tc3xx_ocds_setup tc37x
#
# Everything derivative dependent comes from the TriCore targets of the chip: DBGSR = -dbgbase +
# 0x1FD00, and CBS_TRCx, STMx and SCU_WDTCPUx are indexed by -coreid. Board and application policy
# (adapter speed, DFLASH banks, MCS modules, HSM start, extra diagnostics) belongs in a project cfg,
# see tc37x_ocds_init.cfg.
#
# Options (global variables, set before tc3xx_ocds_setup):
#   tc3xx_hsm_target    1: create <chip>.hsm (cortex_m, aurix-dbg only). Default: 1 with aurix-dbg.
#   tc3xx_mcs_modules   GTM MCS modules whose channels become targets <chip>.mcs<m>.ch<x> (aurix-dbg
#                       only). Default: none.
#   tc3xx_mcs_ramsize   MCS RAM size in bytes per module (-ramsize), if it differs from aurix-dbg's
#                       default (12 KB, TC37x).
#   tc3xx_irq_mask      step kinds run with interrupts masked (aurix-dbg). Default: step step_over
#                       step_out. {} leaves interrupts enabled.
#   tc3xx_tl1_step      1: TL1-gated single step, all cores run for the stepped instruction
#                       (aurix-dbg). Default: 1.
#   tc3xx_hsm_autostart 1: start the HSM after a halting reset (see tc3xx_hsm_release). Default: 1.
#   tc3xx_hsm_ready_ms  how long to wait for the HSM to report ready. Default: 2000.
#   tc3xx_gtm_suspend   1: soft-suspend the GTM while TL1 holds the cores. Default: 0.
#   tc3xx_step_timers   run|frozen, see tc3xx_step_timers. Default: run.
#
# The multi-core break runs on the MCU (OTGS trigger line TL1), not in the debugger - a software sync
# lags one poll plus several TAS round trips, long enough for the other cores to trigger a reset.
#  - CBS_TRCx.BT1: BRKOUT of every CPU (and the HSM) is routed to TL1. A breakpoint (TRxEVT
#    EVTA=2 "halt and pulse BRKOUT") therefore activates TL1 in the same cycle.
#  - CBS_TLCHE.TL1: capture-and-hold, so the BRKOUT pulse keeps TL1 active.
#  - CBS_TL1ST=0xB000003F: CPU0-5, HSSL0, DMA and HSM are TL1 suspend targets - all of them stop
#    while TL1 is active.
#  - pause: a DBGSR halt write is not a debug event and pulses no BRKOUT. With
#    "tricore tl1_halt on", the debugger first pulses TL1 through CBS_TLC
#    (third_party/openocd/BUILD_NOTES.md #10), so a pause also stops every core in the same cycle.
#  - continue: resume-start releases the other cores' debug halt (they stay suspended by the held
#    TL1), the debugger releases its own core, resume-end clears the TL1 hold (CBS_TLCHS) and all
#    cores start in the same cycle.
#  - single step (aurix-dbg, "tricore tl1_step on"): step-start releases the other cores' debug halt
#    while the latched TL1 still holds them; aurix-dbg releases the stepping core and then clears the
#    hold, so all cores run for the one instruction and the step's BRKOUT stops all of them in the
#    same cycle.
#    Without tl1_step (OpenOCD): the other cores are put into a debug halt, then the hold is cleared
#    so only this core executes; the step's breakpoint event latches TL1 again.
#  - STMs: an STM with OCS.SUS=2 stops while its CPU is suspended by TL1 or has DBGSR.SUSP set
#    (measured on the TC377). "tc3xx_step_timers run" (default) leaves SUSP clear, so the STMs stop
#    and run exactly with the cores - also during a TL1-gated step. "frozen" asserts SUSP while
#    halted, so the STMs also stay stopped while a step executes.
# A core stopped by another core's break is reported to gdb as SIGTRAP (BUILD_NOTES #8), which
# VS Code shows as a plain pause instead of "Exception has occurred. Interrupt".
#
# All logic lives in procs: event bodies are not guaranteed to run at global level.

proc tc3xx_default {name value} {
	global $name
	if {![info exists $name]} {
		set $name $value
	}
}

proc tc3xx_is_aurix_dbg {} {
	return [string match "aurix-dbg*" [version]]
}

tc3xx_default tc3xx_hsm_target    [tc3xx_is_aurix_dbg]
tc3xx_default tc3xx_mcs_modules   {}
tc3xx_default tc3xx_irq_mask      {step step_over step_out}
tc3xx_default tc3xx_tl1_step      1
tc3xx_default tc3xx_hsm_autostart 1
tc3xx_default tc3xx_hsm_ready_ms  2000
tc3xx_default tc3xx_gtm_suspend   0
tc3xx_default tc3xx_step_timers   run

# TC3xx registers shared by all derivatives.
set tc3xx_reg(CBS_TLS)   0xF0000470
set tc3xx_reg(OSTATE)    0xF0000480
set tc3xx_reg(CBS_TLC)   0xF0000490
set tc3xx_reg(CBS_TL1ST) 0xF0000494
set tc3xx_reg(CBS_TLCHE) 0xF0000498
set tc3xx_reg(CBS_TLCHS) 0xF000049C
set tc3xx_reg(GTM_OCS)   0xF019FD38
set tc3xx_reg(RSTSTAT)   0xF0036050
set tc3xx_reg(WDTS_SR)   0xF00362B0
# HSM bridge: DBGBASE selects the HSM address window at 0xF0050000; HSM2HTS is the HSM status.
set tc3xx_reg(HSM_DBGBASE) 0xF0041010
set tc3xx_reg(HSM2HTS)     0xF0040034
set tc3xx_reg(HSM_FP_COMP0) 0xF0052008
set tc3xx_reg(HSM_DFSR)    0xF005ED30
set tc3xx_reg(HSM_DHCSR)   0xF005EDF0

# Per-core registers, indexed by the core's -coreid.
proc tc3xx_cbs_trc {coreid} { return [expr {0xF0000420 + 4 * $coreid}] }
proc tc3xx_stm_base {coreid} { return [expr {0xF0001000 + 0x100 * $coreid}] }
proc tc3xx_wdtcpu_sr {coreid} { return [expr {0xF0036254 + 0xC * $coreid}] }
# CBS_TRHSM follows CBS_TRC0..5.
set tc3xx_reg(CBS_TRHSM) [tc3xx_cbs_trc 6]

proc tc3xx_mww {name addr val} {
	if {[catch {mww $addr $val} err]} {
		echo [format "Warning: %s <- 0x%08x failed: %s" $name $val $err]
	}
}

proc tc3xx_rd {addr} {
	return [lindex [read_memory $addr 32 1] 0]
}

# Creates the HSM and MCS targets and installs the event handlers for the TriCore cores of $chip.
proc tc3xx_ocds_setup {chip} {
	global tc3xx_chip tc3xx_cores tc3xx_dbgsr tc3xx_coreid tc3xx_hsm tc3xx_mcs_targets
	global tc3xx_hsm_target tc3xx_mcs_modules tc3xx_mcs_ramsize tc3xx_irq_mask tc3xx_tl1_step

	set tc3xx_chip $chip
	set tc3xx_cores {}
	foreach t [target names] {
		if {[string match "$chip.*" $t] && [$t cget -type] eq "tricore"} {
			lappend tc3xx_cores $t
			set tc3xx_coreid($t) [expr {[$t cget -coreid]}]
			set tc3xx_dbgsr($t) [format 0x%08X [expr {[$t cget -dbgbase] + 0x1FD00}]]
		}
	}
	if {![llength $tc3xx_cores]} {
		return -code error "tc3xx_ocds_setup: no tricore targets named $chip.* (load the target cfg first)"
	}
	set cpu0 [lindex $tc3xx_cores 0]

	# HSM (Cortex-M3) as debug target, aurix-dbg only: accessed through the HSM bridge debug window
	# (DBGBASE 0xF0041010, window 0xF0050000), which works only while the HSM firmware enables HSM
	# debugging. The HSM is a TL1 suspend target and its BRKOUT is routed to TL1 (tc3xx_on_examine),
	# so it halts and resumes together with the TriCore cores:
	#  - HSM breakpoint or pause (tl1_halt): TL1 stops the TriCore cores.
	#  - TriCore halt: the TL1 edge halts the HSM through its break-in.
	#  - continue of any core: resume-start releases the HSM and the other cores, resume-end clears
	#    the TL1 hold.
	#  - HSM single step: only the HSM executes (it runs while TL1 holds the TriCore cores).
	set tc3xx_hsm ""
	if {$tc3xx_hsm_target && [tc3xx_is_aurix_dbg]} {
		set tc3xx_hsm $chip.hsm
		target create $tc3xx_hsm cortex_m -coreid 6 -ocmts [$cpu0 cget -ocmts]
	}

	# GTM MCS channels as debug targets, aurix-dbg only. The MCS has no debug hardware: aurix-dbg
	# stops a channel through its enable bit and sets breakpoints as self-jumps in the MCS RAM. The
	# channels are examined once the application enabled the GTM, and are listed by the frontends only
	# after they ran. Breakpoints and halts stop only their channel (see tc3xx_mcs_halt_all).
	set tc3xx_mcs_targets {}
	if {[tc3xx_is_aurix_dbg]} {
		set ramsize {}
		if {[info exists tc3xx_mcs_ramsize]} {
			set ramsize [list -ramsize $tc3xx_mcs_ramsize]
		}
		foreach m $tc3xx_mcs_modules {
			for {set x 0} {$x < 8} {incr x} {
				set t $chip.mcs$m.ch$x
				target create $t gtm_mcs -module $m -channel $x -coreid [expr {0x40 + 8 * $m + $x}] {*}$ramsize
				lappend tc3xx_mcs_targets $t
			}
		}
	}
	targets $cpu0

	# examine-end fires at init and on every re-examine after a detected reset (OCDS already
	# re-enabled by then).
	$cpu0 configure -event examine-end tc3xx_on_examine
	$cpu0 configure -event reset-end tc3xx_on_reset_end

	foreach t $tc3xx_cores {
		if {[catch {$t tricore tl1_halt on} err]} {
			echo "Warning: $t: 'tricore tl1_halt' not available (OpenOCD not rebuilt?) - pause will not stop the other cores: $err"
		}
		# aurix-dbg: steps of the selected kinds run with interrupts masked (ICR.IE = 0), like
		# winIDEA's DisableIRQ_Step/StepOver/RunUntilReturn. Continue runs with them enabled.
		# OpenOCD has no such command, hence the catch.
		if {[llength $tc3xx_irq_mask]} {
			catch {$t tricore irq_mask {*}$tc3xx_irq_mask}
		}
		# aurix-dbg: TL1-gated single step (all cores run for the stepped instruction, see step-start).
		if {$tc3xx_tl1_step} {
			catch {$t tricore tl1_step on}
		}
		$t configure -event halted       "tc3xx_on_halted $t"
		$t configure -event resume-start "tc3xx_on_resume_start $t"
		$t configure -event resume-end   "tc3xx_on_resume_end $t"
		$t configure -event step-start   "tc3xx_on_step_start $t"
	}

	if {$tc3xx_hsm ne ""} {
		# A pause of the HSM also stops the TriCore cores. Steps run with interrupts masked, as on
		# the TriCore cores. No step-start handler: the HSM steps alone while TL1 holds the others.
		$tc3xx_hsm cortex_m tl1_halt on
		if {[llength $tc3xx_irq_mask]} {
			$tc3xx_hsm cortex_m irq_mask {*}$tc3xx_irq_mask
		}
		$tc3xx_hsm configure -event halted       "tc3xx_on_halted $tc3xx_hsm"
		$tc3xx_hsm configure -event resume-start "tc3xx_on_resume_start $tc3xx_hsm"
		$tc3xx_hsm configure -event resume-end   "tc3xx_on_resume_end $tc3xx_hsm"
	}
}

proc tc3xx_others {self} {
	global tc3xx_cores
	set others {}
	foreach t $tc3xx_cores {
		if {$t ne $self} { lappend others $t }
	}
	return $others
}

# STMx (OCS.SUS=2) is suspended while CPUx is suspended by TL1 (DBGSR.SIH=1) or its suspend-out
# DBGSR.SUSP is set (measured on the TC377 with raw writes: TL1 halt with SUSP=0 stops all STMs, also
# the one of a boot-halted CPU; SUSP=1 on a running CPU stops its STM; clearing the TL1 hold restarts
# them). DBGSR.SUSP (bit 4, rw) is written by every DBGSR write; HALT[2:1]=00 leaves the halt state
# unchanged. A debug event loads SUSP from the event's own SUSP bit (TRxEVT.SUSP), a TL1 halt does not
# change it (EXEVT.SUSP has no effect there).
proc tc3xx_susout {t on} {
	global tc3xx_dbgsr
	tc3xx_mww "$t DBGSR.SUSP" $tc3xx_dbgsr($t) [expr {$on ? 0x00000010 : 0x00000000}]
}

# Suspends STMx with CPUx (OCS.SUS=2) for every core. An application may restrict STMx_ACCEN0 to its
# CPU after start-up: then the debugger write fails with a bus error, but right after a reset ACCEN0
# is still open and the write lands (OCS keeps it). Each write is caught so one locked STM does not
# skip the others.
proc tc3xx_stm_ocs_init {} {
	global tc3xx_cores tc3xx_coreid
	foreach t $tc3xx_cores {
		set i $tc3xx_coreid($t)
		set ocs [expr {[tc3xx_stm_base $i] + 0xE8}]
		set accen [expr {[tc3xx_stm_base $i] + 0xFC}]
		if {[catch {mww $ocs 0x12000000}]} {
			set a "?"
			catch {set a [format 0x%08x [tc3xx_rd $accen]]}
			echo "Warning: STM${i}_OCS write refused (STM${i}_ACCEN0=$a) - app locked it; suspend applies after the next reset"
		} elseif {([tc3xx_rd $ocs] & 0x0F000000) == 0} {
			echo "Warning: STM${i}_OCS write did not stick"
		} else {
			echo "STM$i suspend (OCS.SUS=2) applied"
		}
	}
}

proc tc3xx_on_examine {} {
	global tc3xx_cores tc3xx_coreid tc3xx_reg tc3xx_gtm_suspend
	# A warm reset keeps OCDS/CBS state (no "[OCMTS] Debug (OCDS) enabled" on re-examine). A TL1
	# force or a latched TL1 hold left over from before the reset keeps every core suspended while
	# it boots, and an external watchdog then fires a second (PORST) reset. Drop both first.
	tc3xx_mww CBS_TLC   $tc3xx_reg(CBS_TLC)   0x00000000 ;# no TL1 force
	tc3xx_mww CBS_TLCHS $tc3xx_reg(CBS_TLCHS) 0x00000002 ;# clear a latched TL1 hold
	foreach t $tc3xx_cores { tc3xx_susout $t 0 }

	# MCU-side multi-core break. Pulsing TL1 via CBS_TLC here (as some winIDEA init scripts do) is
	# left out on purpose: with capture-and-hold enabled that would suspend every core right away.
	tc3xx_mww CBS_TL1ST $tc3xx_reg(CBS_TL1ST) 0xB000003F ;# CPU0-5, HSSL0, DMA, HSM are TL1 suspend targets
	foreach t $tc3xx_cores {
		set i $tc3xx_coreid($t)
		tc3xx_mww CBS_TRC$i [tc3xx_cbs_trc $i] 0x00000100 ;# BT1: CPUx BRKOUT -> TL1
	}
	tc3xx_mww CBS_TRHSM $tc3xx_reg(CBS_TRHSM) 0x00000100 ;# BT1: HSM BRKOUT -> TL1
	tc3xx_mww CBS_TLCHE $tc3xx_reg(CBS_TLCHE) 0x00000002 ;# TL1 capture-and-hold

	tc3xx_stm_ocs_init

	# The GTM (timers, PWM outputs, MCS channels) keeps running while the cores are halted.
	# tc3xx_gtm_suspend 1 soft-suspends it (GTM_OCS.SUS=2) while TL1 holds the cores; the GTM outputs
	# then keep their level. Measured on the TC377: OCS.SUSSTA is set while TL1 holds the cores, the
	# TBU time base and the MCS channels stop. One application reset the device right after the
	# continue of a ~100 ms halt with the GTM suspended. GTM_OCS is only cleared by a debug reset:
	# write 0x10000000 to switch the suspend off again.
	if {$tc3xx_gtm_suspend} {
		tc3xx_mww GTM_OCS $tc3xx_reg(GTM_OCS) 0x12000000
	}
}

# STMs while stepping (aurix-dbg TL1-gated steps):
#  run    - SUSP stays clear: TL1 stops and restarts the STMs together with the cores, so every STM
#           advances while the cores execute a step (default).
#  frozen - SUSP is asserted on every core while halted: the STMs also stay stopped during steps.
# At runtime: monitor tc3xx_step_timers frozen|run
proc tc3xx_step_timers {{mode ""}} {
	global tc3xx_step_timers
	if {$mode ne ""} {
		if {[lsearch -exact {run frozen} $mode] < 0} {
			return -code error "usage: tc3xx_step_timers ?run|frozen?"
		}
		set tc3xx_step_timers $mode
	}
	return $tc3xx_step_timers
}

# DBGSR.SUSP value for halted cores in the current mode.
proc tc3xx_halt_susp {} {
	global tc3xx_step_timers
	return [expr {$tc3xx_step_timers eq "frozen" ? 0x00000010 : 0x00000000}]
}

# Any core halted: TL1 has already stopped all cores and, with them, their STMs. "frozen" also
# asserts every core's suspend-out, so the STMs stay stopped while a step executes.
proc tc3xx_on_halted {self} {
	global tc3xx_cores
	if {[tc3xx_halt_susp]} {
		foreach t $tc3xx_cores { tc3xx_susout $t 1 }
	}
}

# Before the debugger releases $self: release the other cores' debug halt too (HALT[2:1]=10, SUSP=0).
# They and their STMs stay suspended by the held TL1 until resume-end.
proc tc3xx_on_resume_start {self} {
	global tc3xx_dbgsr tc3xx_hsm
	foreach t [tc3xx_others $self] {
		tc3xx_mww "$t DBGSR" $tc3xx_dbgsr($t) 0x00000004
	}
	if {$self ne $tc3xx_hsm} {
		tc3xx_hsm_resume
	}
}

# Releases a halted HSM (breakpoint, pause or TL1 break-in) for a continue of a TriCore core: a
# halted HSM keeps its BRKOUT active, which would latch TL1 again right after resume-end. Clears the
# sticky halt reasons (DFSR) as aurix-dbg does for its own resumes.
proc tc3xx_hsm_resume {} {
	global tc3xx_hsm tc3xx_reg
	if {$tc3xx_hsm eq ""} {
		return
	}
	if {[catch {
		mww $tc3xx_reg(HSM_DBGBASE) 0xE0000000 ;# HSM debug area 0xE000xxxx at 0xF005xxxx
		set dhcsr [tc3xx_rd $tc3xx_reg(HSM_DHCSR)]
	}]} {
		return
	}
	if {$dhcsr & 0x00020000} {
		tc3xx_mww "HSM DFSR" $tc3xx_reg(HSM_DFSR) 0x0000001F
		tc3xx_mww "HSM DHCSR" $tc3xx_reg(HSM_DHCSR) 0xA05F0001 ;# debug key, C_DEBUGEN, C_HALT cleared -> run
	}
}

# After the debugger released $self: clear every suspend-out while TL1 still holds the cores, then
# drop the TL1 hold - every core and every STM starts in the same cycle.
proc tc3xx_on_resume_end {self} {
	global tc3xx_cores tc3xx_reg
	foreach t $tc3xx_cores { tc3xx_susout $t 0 }
	tc3xx_mww CBS_TLCHS $tc3xx_reg(CBS_TLCHS) 0x00000002
}

# Single step of $self.
#  - tl1_step (aurix-dbg): release the other cores' debug halt (HALT[2:1]=10) while the latched TL1
#    still suspends them, and set every core's SUSP for the tc3xx_step_timers mode. aurix-dbg
#    releases $self (keeping SUSP, also in the step trigger) and clears the TL1 hold afterwards, so
#    all cores - and in "run" mode all STMs - run for the instruction and the step's BRKOUT stops
#    them in the same cycle.
#  - otherwise (OpenOCD): put the other cores into a real debug halt (HALT[2:1]=11, SUSP=1) so they
#    do not run once the hold is gone, then clear the hold so only $self executes. SUSP=1 on every
#    core keeps the STMs of the debug-halted cores (no longer suspended by TL1) stopped.
# The current setting is queried on every step, so "tricore tl1_step off" at runtime takes effect.
proc tc3xx_on_step_start {self} {
	global tc3xx_dbgsr tc3xx_reg
	set gated [expr {![catch {$self tricore tl1_step} mode] && $mode eq "tl1_step on"}]
	if {$gated} {
		set susp [tc3xx_halt_susp]
		foreach t [tc3xx_others $self] {
			tc3xx_mww "$t DBGSR" $tc3xx_dbgsr($t) [expr {0x00000004 | $susp}]
		}
		tc3xx_mww "$self DBGSR.SUSP" $tc3xx_dbgsr($self) $susp
	} else {
		foreach t [tc3xx_others $self] {
			tc3xx_mww "$t DBGSR" $tc3xx_dbgsr($t) 0x00000016
		}
		tc3xx_susout $self 1
		tc3xx_mww CBS_TLCHS $tc3xx_reg(CBS_TLCHS) 0x00000002
	}
}

# Latches TL1 like a pause: every TL1 suspend target stops until a continue clears the hold.
proc tc3xx_tl1_latch {} {
	global tc3xx_reg
	tc3xx_mww CBS_TLC $tc3xx_reg(CBS_TLC) 0x00000030 ;# force TL1: latched by capture-and-hold
	tc3xx_mww CBS_TLC $tc3xx_reg(CBS_TLC) 0x00000000
}

# HSM start after a halting reset. With a debugger connected, the boot ROM puts an on-chip breakpoint
# on the HSM's first instruction (Vector vHsm_IntegrationHints 3.7.3), so after a halting reset the
# HSM stays halted. If the startup software waits for the HSM (HSM2HTS), it waits forever.
# tc3xx_hsm_release deletes that breakpoint and runs the HSM while cpu0 is still halted at its first
# instruction - no race. Measured on the TC377:
#  - TL1 halts the HSM through its break-in on the TL1 edge and resumes it with the cores, so the
#    HSM is released after TL1 has been latched and then keeps running while the cores are held.
#  - The SSW may wait for the HSM (PROCONHSMCFG.SSWWAIT); starting the HSM together with cpu0 can
#    send the startup software into an error path. So wait until HSM2HTS reports READY|ACTIVE (0x3)
#    before cpu0 may run (at most tc3xx_hsm_ready_ms).
# If the release fails, the halted HSM's BRKOUT stays active and, routed to TL1, would re-latch the
# hold after every continue: the HSM routing is switched off instead.
# set tc3xx_hsm_autostart 0 to leave the HSM halted (debugging the HSM with another tool).
proc tc3xx_hsm_release {} {
	global tc3xx_reg
	if {[catch {
		mww $tc3xx_reg(HSM_DBGBASE) 0xE0000000  ;# HSM debug area 0xE000xxxx at 0xF005xxxx
		mww $tc3xx_reg(HSM_FP_COMP0) 0x00000000 ;# delete the boot ROM breakpoint
		mww $tc3xx_reg(HSM_DHCSR) 0xA05F0001    ;# debug key, C_DEBUGEN, C_HALT cleared -> run
		set dhcsr [tc3xx_rd $tc3xx_reg(HSM_DHCSR)]
	} err]} {
		echo "Warning: HSM not released (HSM debug access: $err)"
		return 0
	}
	if {$dhcsr & 0x00020000} {
		echo [format "Warning: HSM still halted (DHCSR=0x%08x)" $dhcsr]
		return 0
	}
	return 1
}

# Waits until the HSM reports READY|ACTIVE in HSM2HTS, like the SSW does with SSWWAIT.
proc tc3xx_hsm_wait_ready {} {
	global tc3xx_hsm_ready_ms tc3xx_reg
	set v 0
	for {set t 0} {$t <= $tc3xx_hsm_ready_ms} {incr t 20} {
		catch {set v [tc3xx_rd $tc3xx_reg(HSM2HTS)]}
		if {($v & 0x3) == 0x3} {
			return 1
		}
		sleep 20
	}
	echo [format "Warning: HSM not ready after %d ms (HSM2HTS=0x%08x)" $tc3xx_hsm_ready_ms $v]
	return 0
}

# A debugger reset (reset run|halt|init, VS Code Reset) clears the STM OCS and does not re-examine,
# so examine-end does not fire (OpenOCD fires it before asserting the reset). Redo the setup at
# reset-end. After a halting reset ACCEN0 is still open, so the STM suspend lands. cpu0 is only
# debug halted and the other cores are in boot halt, which stops no STM: latch TL1 like a pause, so
# every core and STM is suspended until continue releases them (resume-start/resume-end).
proc tc3xx_on_reset_end {} {
	global tc3xx_cores tc3xx_hsm_autostart tc3xx_reg
	tc3xx_on_examine
	set cpu0 [lindex $tc3xx_cores 0]
	if {[$cpu0 curstate] eq "halted"} {
		tc3xx_tl1_latch
		# After the TL1 edge, so TL1 does not halt the HSM again.
		if {$tc3xx_hsm_autostart && [tc3xx_hsm_release]} {
			tc3xx_hsm_wait_ready
		} else {
			tc3xx_mww CBS_TRHSM $tc3xx_reg(CBS_TRHSM) 0x00000000 ;# the halted HSM keeps BRKOUT active
		}
		tc3xx_on_halted $cpu0
	}
}

# Connect with a halting reset (launch.json "configCommands": ["init", "tc3xx_connect_reset_halt"]).
# An application that locks STMx_ACCEN after start-up only lets the STM suspend land while the cores
# are halted right after a reset. A watchdog can reset the chip again and the halt is lost: cpu0 must
# still be halted a moment later, otherwise retry after $delay ms. Only warns when every attempt
# fails, so the debug session still starts.
proc tc3xx_connect_reset_halt {{tries 3} {delay 200}} {
	global tc3xx_cores tc3xx_dbgsr
	set cpu0 [lindex $tc3xx_cores 0]
	for {set i 1} {$i <= $tries} {incr i} {
		if {[catch {reset halt} err]} {
			echo "Warning: reset halt attempt $i: $err"
		} else {
			sleep 100
			set halted 0
			catch {set halted [expr {[tc3xx_rd $tc3xx_dbgsr($cpu0)] & 0x2}]}
			if {$halted} {
				echo "Connected with reset halt (attempt $i)"
				return
			}
			echo "Warning: reset halt attempt $i: $cpu0 no longer halted (watchdog reset?)"
		}
		sleep $delay
	}
	echo "Warning: reset halt failed $tries times - cores not halted after reset"
}

# An MCS breakpoint stops only its channel: the cores and the other MCS channels keep running.
# tc3xx_mcs_halt_all also halts all cores (TL1, like a pause) when a channel stops at a breakpoint or
# a step. Halts of a channel the application disables itself are ignored. Continue the MCS channel
# separately: a TriCore continue does not enable MCS channels. To use it:
#   foreach t $tc3xx_mcs_targets { $t configure -event halted "tc3xx_mcs_halt_all $t" }
proc tc3xx_mcs_halt_all {self} {
	if {[lsearch -exact {breakpoint single-step} [$self debug_reason]] < 0} {
		return
	}
	tc3xx_tl1_latch
}

# Diagnostic, while halted: monitor tc3xx_stm_status in the VS Code debug console.
proc tc3xx_stm_status {} {
	global tc3xx_cores tc3xx_dbgsr tc3xx_coreid tc3xx_reg
	set tls [tc3xx_rd $tc3xx_reg(CBS_TLS)]
	echo [format "CBS: TLS.TL1=%d  TLC=0x%08x  TL1ST=0x%08x  TLCHE=0x%08x" \
		[expr {($tls >> 1) & 1}] [tc3xx_rd $tc3xx_reg(CBS_TLC)] \
		[tc3xx_rd $tc3xx_reg(CBS_TL1ST)] [tc3xx_rd $tc3xx_reg(CBS_TLCHE)]]
	foreach t $tc3xx_cores {
		set i $tc3xx_coreid($t)
		set dbg  [tc3xx_rd $tc3xx_dbgsr($t)]
		set base [tc3xx_stm_base $i]
		set ocs  [tc3xx_rd [expr {$base + 0xE8}]]
		set t1   [tc3xx_rd [expr {$base + 0x10}]]
		sleep 100
		set t2   [tc3xx_rd [expr {$base + 0x10}]]
		echo [format "%s DBGSR=0x%08x (HALT=%d SIH=%d SUSP=%d EVTSRC=0x%02x)  STM%d OCS.SUS=%d SUSSTA=%d  TIM0 %s" \
			$t $dbg [expr {($dbg >> 1) & 3}] [expr {($dbg >> 3) & 1}] [expr {($dbg >> 4) & 1}] \
			[expr {($dbg >> 8) & 0x1F}] $i [expr {($ocs >> 24) & 0xF}] [expr {($ocs >> 29) & 1}] \
			[expr {$t1 == $t2 ? "stopped" : "RUNNING"}]]
	}
}

# Diagnostic: internal watchdogs and the cause of the last reset (monitor tc3xx_wdt_status).
#  - CBS_OSTATE.WDTSUS (bit 7): 0 = all watchdogs suspended while OCDS is enabled (reset default),
#    1 = a CPU watchdog is only suspended while that CPU's SUSOUT (DBGSR.SUSP) is asserted.
#  - SCU_WDTxSR: TIM [31:16] counts while the watchdog runs; DS (bit 3) disabled, OE (bit 1)
#    overflow error, AE (bit 0) access error.
#  - SCU_RSTSTAT: trigger of the last reset. A CPU/safety watchdog timeout ends as an SMU reset,
#    an external watchdog as ESR0/ESR1 or PORST.
proc tc3xx_wdt_status {} {
	global tc3xx_cores tc3xx_dbgsr tc3xx_coreid tc3xx_reg
	set ostate [tc3xx_rd $tc3xx_reg(OSTATE)]
	set wdtsus [expr {($ostate >> 7) & 1}]
	echo [format "OSTATE=0x%08x OEN=%d WDTSUS=%d: %s" $ostate [expr {$ostate & 1}] $wdtsus \
		[expr {$wdtsus ? "a watchdog is only suspended while its CPU's SUSOUT is asserted" : "all watchdogs suspended while OCDS is enabled"}]]
	set wdts {}
	foreach t $tc3xx_cores {
		lappend wdts WDTCPU$tc3xx_coreid($t) [tc3xx_wdtcpu_sr $tc3xx_coreid($t)] $t
	}
	lappend wdts WDTS $tc3xx_reg(WDTS_SR) ""
	foreach {name addr t} $wdts {
		set sr1 [tc3xx_rd $addr]
		sleep 100
		set sr2 [tc3xx_rd $addr]
		set susout ""
		if {$t ne ""} {
			set susout [format "  SUSOUT=%d" [expr {([tc3xx_rd $tc3xx_dbgsr($t)] >> 4) & 1}]]
		}
		echo [format "%-7s SR=0x%08x TIM=0x%04x %-8s DS=%d OE=%d AE=%d%s" $name $sr2 [expr {($sr2 >> 16) & 0xFFFF}] \
			[expr {(($sr1 ^ $sr2) >> 16) & 0xFFFF ? "COUNTING" : "stopped"}] \
			[expr {($sr2 >> 3) & 1}] [expr {($sr2 >> 1) & 1}] [expr {$sr2 & 1}] $susout]
	}
	set rst [tc3xx_rd $tc3xx_reg(RSTSTAT)]
	set causes {}
	foreach {bit name} {0 ESR0 1 ESR1 3 SMU 4 SW 5 STM0 6 STM1 7 STM2 16 PORST 18 CB0 19 CB1 20 CB3 23 EVRC 24 EVR33 25 SWD 28 STBYR} {
		if {($rst >> $bit) & 1} { lappend causes $name }
	}
	echo [format "RSTSTAT=0x%08x last reset: %s" $rst [expr {[llength $causes] ? [join $causes ", "] : "-"}]]
}
