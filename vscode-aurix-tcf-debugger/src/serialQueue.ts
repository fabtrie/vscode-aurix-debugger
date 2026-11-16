// Jobs that must not overlap, run in the order they were queued. No vscode import, so the tests
// can check it directly.

export class SerialQueue {
	private tail: Promise<unknown> = Promise.resolve();

	/** Runs `job` once the jobs queued before it have finished, whether they failed or not. */
	run<T>(job: () => Promise<T>): Promise<T> {
		const result = this.tail.then(job, job);
		this.tail = result.catch(() => undefined);
		return result;
	}
}
