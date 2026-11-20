/**
 * What VS Code remembers of the stops it was told about, so the adapter knows which halted threads
 * VS Code would show as running after it rebuilds its threads.
 *
 * VS Code keeps a list of the StoppedEvents it received and drops a thread's entries when that
 * thread continues. An InvalidatedEvent for `stacks` (or `threads`, `all`) makes it throw away all
 * threads, fetch them again and re-apply only the remembered stops; a thread no remembered stop
 * covers comes back as running.
 *
 * With TL1 the other cores are usually stopped by the stopping core's `allThreadsStopped`: their
 * own stops are not reported. Once that core continues (a step, a Step Back), its entry is gone and
 * the other cores' halt is no longer remembered anywhere - although they stay halted.
 */
export class StoppedDetailsMirror {
	private stops: Array<{ threadId: number; all: boolean }> = [];

	/** A StoppedEvent was sent. */
	stopped(threadId: number, allThreadsStopped: boolean): void {
		this.stops.push({ threadId, all: allThreadsStopped });
	}

	/** A ContinuedEvent with allThreadsContinued false was sent. */
	continued(threadId: number): void {
		this.stops = this.stops.filter(s => s.threadId !== threadId);
	}

	clear(): void {
		this.stops = [];
	}

	/** Whether a rebuild of VS Code's threads shows this thread as stopped. */
	covers(threadId: number): boolean {
		return this.stops.some(s => s.all || s.threadId === threadId);
	}

	/** The halted threads a rebuild would show as running: they need a stop of their own first. */
	uncovered(haltedThreadIds: Iterable<number>): number[] {
		return Array.from(haltedThreadIds).filter(id => !this.covers(id));
	}
}
